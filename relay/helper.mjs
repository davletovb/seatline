#!/usr/bin/env node
import { spawn } from "node:child_process";
import { readFileSync, openSync, closeSync, unlinkSync } from "node:fs";
import { join } from "node:path";
import { randomBytes, randomUUID } from "node:crypto";
import { importKey, seal, open } from "./crypto.mjs";

const [app, relayUrl, website] = process.argv.slice(2);
if (!/^[a-z0-9][a-z0-9_-]{0,63}$/.test(app ?? "")) throw new Error("Usage: helper.mjs APP RELAY_URL WEBSITE_URL");
const relay = new URL(relayUrl);
const site = new URL(website);
if (relay.protocol !== "https:" || site.protocol !== "https:" || relay.username || relay.password || relay.search || relay.hash || site.username || site.password) throw new Error("Pairing requires HTTPS URLs");
const root = process.env.SEATLINE_DATA_DIR ?? join(process.platform === "win32" ? process.env.LOCALAPPDATA : join(process.env.XDG_DATA_HOME ?? join(process.env.HOME, ".local/share")), "seatline");
const grantPath = join(root, "apps", `${app}.json`);
const grant = JSON.parse(readFileSync(grantPath, "utf8"));
if (grant.app !== app || grant.worker?.protocol !== "rpc" || !grant.web_origins?.includes(site.origin)) throw new Error("This website has not been authorized for the app");
// A single managed worker owns the app engine, including runs and recovery.
const lockPath = join(root, `${app}-relay.lock`);
let lock;
try { lock = openSync(lockPath, "wx", 0o600); }
catch { throw new Error("This app already has a relay helper. Close it before pairing again."); }
const result = await fetch(new URL("/pair", relay), { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ app, origin: site.origin }), signal: AbortSignal.timeout(15_000) }).catch(error => { closeSync(lock); unlinkSync(lockPath); throw error; });
if (!result.ok) { closeSync(lock); unlinkSync(lockPath); throw new Error(`Pairing failed (${result.status})`); }
const pair = await result.json();
if (![pair.id, pair.helper, pair.browser].every(s => typeof s === "string" && /^[a-f0-9]{64}$/.test(s))) throw new Error("Invalid relay pairing response");
const keyBytes = randomBytes(32);
const key = await importKey(keyBytes);
site.hash = `seatline=${pair.id}:${pair.browser}:${keyBytes.toString("base64url")}`;
console.log(`Open Conclave with this private pairing link:\n${site.href}`);
if (process.env.SEATLINE_OPEN_BROWSER === "1") {
  const opener = process.platform === "win32" ? ["rundll32", ["url.dll,FileProtocolHandler", site.href]] : process.platform === "darwin" ? ["open", [site.href]] : ["xdg-open", [site.href]];
  spawn(opener[0], opener[1], { stdio: "ignore", windowsHide: true }).unref();
}
const worker = spawn(grant.worker.executable, grant.worker.args, { stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
worker.stderr.pipe(process.stderr);
let socket;
let peer = false;
let stopped = false;
let received = 0;
let sequence = 0;
let buffer = Buffer.alloc(0);
let outgoing = Promise.resolve();
let incoming = Promise.resolve();
let queued = 0;
const workerSend = value => {
  const bytes = Buffer.from(JSON.stringify(value));
  if (bytes.length > 1024 * 1024 || worker.stdin.writableLength > 2 * 1024 * 1024) throw new Error("Worker input queue full");
  const frame = Buffer.alloc(bytes.length + 4); frame.writeUInt32LE(bytes.length, 0); bytes.copy(frame, 4); worker.stdin.write(frame);
};
const disconnected = () => {
  peer = false;
  // Drop subscriptions, retain orchestration. The website replays by sequence.
  try { workerSend({ id: randomUUID(), type: "disconnect" }); } catch { stop(); }
};
worker.stdout.on("data", chunk => {
  buffer = Buffer.concat([buffer, chunk]);
  while (buffer.length >= 4) {
    const length = buffer.readUInt32LE(0);
    if (!length || length > 1024 * 1024) { stop(); return; }
    if (buffer.length < length + 4) return;
    let value; try { value = JSON.parse(buffer.subarray(4, length + 4).toString("utf8")); } catch { stop(); return; }
    buffer = buffer.subarray(length + 4);
    if (value.type === "ready" || !peer) continue;
    const target = socket;
    if (++queued > 128) { disconnected(); target?.close(); queued--; continue; }
    outgoing = outgoing.then(async () => {
      if (target !== socket || !peer || target.readyState !== WebSocket.OPEN) return;
      if (target.bufferedAmount > 4 * 1024 * 1024) { disconnected(); target.close(); return; }
      target.send(JSON.stringify(await seal(key, "helper", ++sequence, value)));
    }).catch(() => { disconnected(); target?.close(); }).finally(() => queued--);
  }
});
function connect() {
  if (stopped) return;
  const endpoint = new URL(`/channels/${pair.id}/helper`, relay); endpoint.protocol = "wss:";
  const current = new WebSocket(endpoint); socket = current;
  current.addEventListener("open", () => current.send(JSON.stringify({ type: "auth", token: pair.helper })));
  current.addEventListener("message", event => {
    if (typeof event.data !== "string" || event.data.length > 768 * 1024) { current.close(); return; }
    incoming = incoming.then(async () => {
      if (current !== socket) return;
      const value = JSON.parse(event.data);
      if (value.type === "ready" || value.type === "peer") { peer = value.peer === true || value.connected === true; if (!peer) disconnected(); return; }
      if (value.type === "pong") return;
      if (value.type !== "data" || value.seq <= received) return;
      const request = await open(key, "browser", value);
      received = value.seq;
      // Only API messages enter the pre-authorized worker. Never executable paths.
      workerSend(request);
    }).catch(() => current.close(1008, "Invalid encrypted message"));
  });
  current.addEventListener("close", event => { if (current !== socket) return; disconnected(); if (event.code === 1008) stop(); else setTimeout(connect, 1000); });
  current.addEventListener("error", () => current.close());
}
const health = setInterval(() => {
  try { if (JSON.parse(readFileSync(grantPath, "utf8")).token !== grant.token) { stop(); return; } }
  catch { stop(); return; }
  if (socket?.readyState === WebSocket.OPEN) socket.send('{"type":"ping"}');
}, 5000);
function stop() {
  if (stopped) return;
  stopped = true; clearInterval(health); socket?.close(); worker.stdin.end(); worker.kill();
  try { closeSync(lock); unlinkSync(lockPath); } catch { /* already removed */ }
}
worker.once("error", error => { console.error(`Seatline worker unavailable: ${error.message}`); stop(); });
worker.once("exit", stop);
process.once("SIGTERM", stop); process.once("SIGINT", stop); process.once("exit", stop);
connect();
