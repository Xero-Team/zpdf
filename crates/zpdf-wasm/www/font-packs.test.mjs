import assert from "node:assert/strict";
import { createHash, webcrypto } from "node:crypto";
import { test } from "node:test";
import { registerFontPack } from "./font-packs.mjs";

globalThis.crypto ??= webcrypto;
const bytes = new Uint8Array([1, 2, 3]);
const sha256 = createHash("sha256").update(bytes).digest("hex");
const options = { url: "https://example.invalid/font.zlib", sha256,
  alias: "ApplicationCjk", decodedLimit: 100, maxEncodedBytes: 3 };

function cache() {
  const entries = new Map();
  globalThis.caches = { open: async () => ({
    match: async key => entries.get(key)?.clone(),
    put: async (key, value) => entries.set(key, value),
    delete: async key => entries.delete(key),
  }) };
  return entries;
}

test("verified bytes are reused offline and registered with explicit limits", async () => {
  cache();
  let fetched = 0;
  let registered = 0;
  globalThis.fetch = async (_url, init) => {
    assert.equal(init.credentials, "omit");
    assert.equal(init.redirect, "error");
    fetched++;
    return new Response(bytes);
  };
  const pdf = { register_zlib_font(data, face, alias, limit) {
    assert.deepEqual(data, bytes);
    assert.deepEqual([face, alias, limit], [0, options.alias, 100]);
    return ++registered;
  } };
  assert.equal(await registerFontPack(pdf, options), 1);
  assert.equal(await registerFontPack(pdf, { ...options, offline: true }), 2);
  assert.equal(fetched, 1);
});

test("oversized or incorrect responses never reach the renderer or cache", async () => {
  for (const response of [new Response(new Uint8Array(4)), new Response(new Uint8Array(3)),
    new Response(bytes, { headers: { "Content-Length": "4" } })]) {
    const entries = cache();
    globalThis.fetch = async () => response;
    const pdf = { register_zlib_font() { assert.fail("invalid bytes were registered"); } };
    await assert.rejects(registerFontPack(pdf, options));
    assert.equal(entries.size, 0);
  }
});

test("offline cache miss or corruption never fetches", async () => {
  const entries = cache();
  globalThis.fetch = async () => assert.fail("offline acquisition fetched");
  await assert.rejects(registerFontPack({}, { ...options, offline: true }));
  const key = new URL(options.url);
  key.searchParams.set("zpdf-font-sha256", sha256);
  entries.set(key.href, new Response(new Uint8Array(3)));
  await assert.rejects(registerFontPack({}, { ...options, offline: true }));
});

test("insecure sources and invalid WASM limits are rejected before I/O", async () => {
  globalThis.caches = { open() { assert.fail("invalid options performed I/O"); } };
  for (const changed of [{ url: "http://example.invalid/font" }, { decodedLimit: -1 },
    { decodedLimit: 0x100000000 }, { maxEncodedBytes: 0 }, { sha256: "x" }, { alias: "" }]) {
    await assert.rejects(registerFontPack({}, { ...options, ...changed }));
  }
});
