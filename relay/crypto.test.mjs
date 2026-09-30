import test from "node:test";
import assert from "node:assert/strict";
import { importKey, seal, open } from "./crypto.mjs";

test("pair messages reject wrong direction, tampering, and other pairing keys", async () => {
  const key = await importKey(crypto.getRandomValues(new Uint8Array(32)));
  const other = await importKey(crypto.getRandomValues(new Uint8Array(32)));
  const value = { id: "run-1", type: "request", body: "Private prompt 🌍" };
  const sealed = await seal(key, "browser", 1, value);
  assert.deepEqual(await open(key, "browser", sealed), value);
  assert.ok(!JSON.stringify(sealed).includes("Private prompt"));
  await assert.rejects(open(key, "helper", sealed));
  await assert.rejects(open(other, "browser", sealed));
  await assert.rejects(open(key, "browser", { ...sealed, seq: 2 }));
  await assert.rejects(open(key, "browser", { ...sealed, body: sealed.body.slice(0, -4) + "AAAA" }));
});
