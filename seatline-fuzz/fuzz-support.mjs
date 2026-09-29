// Seed-corpus generator of the runtime's fuzz targets. It reads the limits it
// seeds from the target's own constants, and needs nothing but Node.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const STREAM_TARGET = path.join(HERE, "fuzz_targets", "stream_lines.rs");

function streamConfig() {
  const source = fs.readFileSync(STREAM_TARGET, "utf8");
  const line = Number(source.match(/const MAX_LINE_BYTES: usize = (\d+);/)?.[1]);
  const schedule = Number(source.match(/const SCHEDULE_BYTES: usize = (\d+);/)?.[1]);
  if (!Number.isSafeInteger(line) || line <= 0 || !Number.isSafeInteger(schedule) || schedule <= 0) {
    throw new Error("stream_lines fuzz limits must be positive integer constants");
  }
  return { line, schedule };
}

function createStreamCorpus(output) {
  fs.mkdirSync(output, { recursive: true });
  const { line, schedule } = streamConfig();
  const control = Buffer.alloc(schedule, 1);
  const seeds = new Map([
    ["at-limit.bin", Buffer.from("a".repeat(line) + "\r\n")],
    ["over-limit.bin", Buffer.from("a".repeat(line + 1) + "\n")],
    ["utf8-crlf.bin", Buffer.from("é\r\nmore\r\nlast\r")],
    ["invalid-utf8.bin", Buffer.from([0xff, 0x0a])],
    ["empty-lines.bin", Buffer.from("\n\r\n\r")],
  ]);
  for (const [name, payload] of seeds) {
    fs.writeFileSync(path.join(output, name), Buffer.concat([control, payload]));
  }
  console.log(`Wrote ${seeds.size} bounded stream fuzz seeds.`);
}

function usage() {
  console.error("usage: fuzz-support.mjs stream-corpus <output> | max-len <stream_lines>");
  process.exit(64);
}

const [command, argument, ...extra] = process.argv.slice(2);
if (!command || !argument || extra.length) usage();

switch (command) {
  case "stream-corpus":
    createStreamCorpus(path.resolve(argument));
    break;
  case "max-len":
    if (argument === "stream_lines") {
      const { line, schedule } = streamConfig();
      console.log(2 * (line + 2) + schedule);
    } else usage();
    break;
  default:
    usage();
}
