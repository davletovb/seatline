const MAX_MESSAGE = 768 * 1024;
const hex = bytes => Array.from(bytes, b => b.toString(16).padStart(2, "0")).join("");
const token = () => hex(crypto.getRandomValues(new Uint8Array(32)));
const equal = (a, b) => typeof a === "string" && a.length === 64 && typeof b === "string" && b.length === 64
  && [...a].reduce((n, c, i) => n | (c.charCodeAt(0) ^ b.charCodeAt(i)), 0) === 0;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname === "/pair" && request.method === "POST") {
      if (Number(request.headers.get("content-length")) > 2048) return new Response("Too large", { status: 413 });
      let input;
      try { const text = await request.text(); if (text.length > 2048) throw Error(); input = JSON.parse(text); }
      catch { return new Response("Invalid pairing request", { status: 400 }); }
      const origins = JSON.parse(env.APP_ORIGINS ?? "{}")[input.app];
      if (!Array.isArray(origins) || !origins.includes(input.origin)) return new Response("App origin not approved", { status: 403 });
      const id = token();
      const credentials = { helper: token(), browser: token() };
      const stub = env.CHANNELS.get(env.CHANNELS.idFromName(id));
      await stub.fetch(new Request("https://internal/init", { method: "POST", body: JSON.stringify({ app: input.app, origin: input.origin, credentials, expires: Date.now() + 24 * 60 * 60_000 }) }));
      return Response.json({ id, ...credentials }, { headers: { "cache-control": "no-store" } });
    }
    const route = /^\/channels\/([a-f0-9]{64})\/(browser|helper)$/.exec(url.pathname);
    if (!route || request.headers.get("upgrade")?.toLowerCase() !== "websocket") return new Response("Not found", { status: 404 });
    const stub = env.CHANNELS.get(env.CHANNELS.idFromName(route[1]));
    return stub.fetch(request);
  },
};

export class Channel {
  constructor(state) { this.state = state; }
  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname === "/init") {
      if (await this.state.storage.get("pair")) return new Response("Exists", { status: 409 });
      const pair = await request.json();
      await this.state.storage.put("pair", pair);
      await this.state.storage.setAlarm(pair.expires);
      return new Response("Created");
    }
    const pair = await this.state.storage.get("pair");
    if (!pair || pair.expires <= Date.now()) return new Response("Pairing expired", { status: 410 });
    const role = url.pathname.endsWith("/helper") ? "helper" : "browser";
    if (role === "browser" && request.headers.get("origin") !== pair.origin) return new Response("Origin refused", { status: 403 });
    const existing = this.state.getWebSockets(role);
    // Allow one unauthenticated replacement, with a short authentication deadline.
    if (existing.filter(ws => !ws.deserializeAttachment()?.authorized).length >= 1) return new Response("Busy", { status: 429 });
    const sockets = new WebSocketPair();
    const [client, server] = Object.values(sockets);
    this.state.acceptWebSocket(server, [role]);
    server.serializeAttachment({ role, authorized: false, opened: Date.now(), bytes: 0, window: Date.now() });
    // Hibernation-safe alarm also retires abandoned unauthenticated sockets.
    await this.state.storage.setAlarm(Math.min(pair.expires, Date.now() + 10_000));
    return new Response(null, { status: 101, webSocket: client });
  }
  async webSocketMessage(socket, message) {
    const attachment = socket.deserializeAttachment();
    if (typeof message !== "string" || message.length > MAX_MESSAGE) { socket.close(1009, "Frame too large"); return; }
    let value;
    try { value = JSON.parse(message); } catch { socket.close(1008, "Invalid frame"); return; }
    const pair = await this.state.storage.get("pair");
    if (!pair || pair.expires <= Date.now()) { socket.close(1008, "Pairing expired"); return; }
    if (!attachment.authorized) {
      if (Date.now() - attachment.opened > 10_000 || value.type !== "auth" || !equal(value.token, pair.credentials[attachment.role])) { socket.close(1008, "Authorization refused"); return; }
      attachment.authorized = true;
      socket.serializeAttachment(attachment);
      for (const old of this.state.getWebSockets(attachment.role)) if (old !== socket) old.close(1000, "Replaced");
      const peers = this.state.getWebSockets(attachment.role === "helper" ? "browser" : "helper").filter(ws => ws.deserializeAttachment()?.authorized);
      socket.send(JSON.stringify({ type: "ready", peer: peers.length > 0 }));
      for (const peer of peers) peer.send(JSON.stringify({ type: "peer", connected: true }));
      return;
    }
    if (value.type === "ping") { socket.send('{"type":"pong"}'); return; }
    if (value.type !== "data" || typeof value.body !== "string" || typeof value.iv !== "string" || !Number.isSafeInteger(value.seq)) { socket.close(1008, "Invalid envelope"); return; }
    if (Date.now() - attachment.window >= 1000) { attachment.window = Date.now(); attachment.bytes = 0; }
    attachment.bytes += message.length;
    if (attachment.bytes > 4 * MAX_MESSAGE) { socket.close(1008, "Rate exceeded"); return; }
    socket.serializeAttachment(attachment);
    const peer = this.state.getWebSockets(attachment.role === "helper" ? "browser" : "helper").find(ws => ws.deserializeAttachment()?.authorized);
    if (!peer) { socket.send('{"type":"peer","connected":false}'); return; }
    try { peer.send(message); } catch { socket.close(1011, "Peer unavailable"); }
  }
  async webSocketClose(socket) {
    const role = socket.deserializeAttachment()?.role;
    if (!role || !socket.deserializeAttachment()?.authorized) return;
    if (this.state.getWebSockets(role).some(ws => ws !== socket && ws.deserializeAttachment()?.authorized && ws.readyState === 1)) return;
    for (const peer of this.state.getWebSockets(role === "helper" ? "browser" : "helper")) {
      if (peer.deserializeAttachment()?.authorized) { try { peer.send('{"type":"peer","connected":false}'); } catch { /* closed */ } }
    }
  }
  async webSocketError(socket) { socket.close(1011, "Connection failed"); }
  async alarm() {
    const pair = await this.state.storage.get("pair");
    if (!pair || pair.expires <= Date.now()) {
      for (const socket of this.state.getWebSockets()) socket.close(1000, "Pairing expired");
      await this.state.storage.deleteAll(); return;
    }
    for (const socket of this.state.getWebSockets()) if (!socket.deserializeAttachment()?.authorized) socket.close(1008, "Authentication timed out");
    await this.state.storage.setAlarm(pair.expires);
  }
}
