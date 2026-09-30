// Protocol 1: direction-bound AES-GCM, monotonic sequence, random 96-bit IV.
export async function importKey(bytes) {
  return crypto.subtle.importKey("raw", bytes, "AES-GCM", false, ["encrypt", "decrypt"]);
}
export async function seal(key, direction, seq, value) {
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const data = new TextEncoder().encode(JSON.stringify(value));
  const aad = new TextEncoder().encode(`seatline:1:${direction}:${seq}`);
  const ciphertext = await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: aad }, key, data);
  return { type: "data", seq, iv: Buffer.from(iv).toString("base64"), body: Buffer.from(ciphertext).toString("base64") };
}
export async function open(key, direction, envelope) {
  if (!Number.isSafeInteger(envelope.seq) || envelope.seq < 1 || typeof envelope.iv !== "string" || typeof envelope.body !== "string") throw new Error("Invalid encrypted envelope");
  const iv = Buffer.from(envelope.iv, "base64");
  if (iv.length !== 12) throw new Error("Invalid IV");
  const aad = new TextEncoder().encode(`seatline:1:${direction}:${envelope.seq}`);
  const clear = await crypto.subtle.decrypt({ name: "AES-GCM", iv, additionalData: aad }, key, Buffer.from(envelope.body, "base64"));
  return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(clear));
}
