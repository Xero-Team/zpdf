// Optional host-side acquisition. Call again and redraw after a font arrives.
// Sources must serve zlib-compressed font bytes with a pinned encoded SHA-256.
// The synchronous Rust rendering path never fetches or accesses CacheStorage.

async function boundedBytes(response, limit) {
  if (!response.ok || !response.body) throw new Error("Font response is unavailable");
  const length = response.headers.get("Content-Length");
  if (length !== null && (!/^\d+$/.test(length) || Number(length) > limit)) {
    await response.body.cancel();
    throw new Error("Font response exceeds the encoded byte limit");
  }
  const reader = response.body.getReader();
  const chunks = [];
  let size = 0;
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > limit) throw new Error("Font response exceeds the encoded byte limit");
      chunks.push(value);
    }
  } catch (error) {
    await reader.cancel();
    throw error;
  } finally {
    reader.releaseLock();
  }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return bytes;
}

async function verifiedBytes(response, sha256, limit) {
  const bytes = await boundedBytes(response, limit);
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  const actual = Array.from(digest, byte => byte.toString(16).padStart(2, "0")).join("");
  if (actual !== sha256) throw new Error("Font pack SHA-256 mismatch");
  return bytes;
}

export async function registerFontPack(pdf, {
  url, sha256, alias, decodedLimit,
  maxEncodedBytes = 64 * 1024 * 1024,
  offline = false,
}) {
  const source = new URL(url);
  if (source.protocol !== "https:" || source.username || source.password || source.hash) {
    throw new Error("Font source must use HTTPS without credentials or a fragment");
  }
  if (!/^[0-9a-f]{64}$/.test(sha256) || !alias
      || !Number.isSafeInteger(decodedLimit) || decodedLimit <= 0 || decodedLimit > 0xffffffff
      || !Number.isSafeInteger(maxEncodedBytes) || maxEncodedBytes <= 0) {
    throw new Error("Invalid font pack identity or byte limits");
  }
  // SHA identity changes create separate cache entries; offline reads are
  // rechecked rather than trusting metadata saved by a previous app version.
  const key = new URL(source);
  key.searchParams.set("zpdf-font-sha256", sha256);
  const cache = await caches.open("zpdf-font-packs-v1");
  const cached = await cache.match(key.href);
  let bytes;
  if (cached) {
    try {
      bytes = await verifiedBytes(cached, sha256, maxEncodedBytes);
    } catch (error) {
      if (offline) throw error;
      await cache.delete(key.href);
    }
  }
  if (!bytes) {
    if (offline) throw new Error("No verified font pack is cached for offline use");
    const response = await fetch(source.href, {
      credentials: "omit", redirect: "error", signal: AbortSignal.timeout(30000),
    });
    bytes = await verifiedBytes(response, sha256, maxEncodedBytes);
    await cache.put(key.href, new Response(bytes));
  }
  return pdf.register_zlib_font(bytes, 0, alias, decodedLimit);
}
