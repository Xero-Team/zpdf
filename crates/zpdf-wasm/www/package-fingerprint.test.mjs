import assert from "node:assert/strict";
import { copyFileSync, mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { sourceFingerprint } from "./package-fingerprint.mjs";

test("changes to embedded assets outside src invalidate the WASM fingerprint", () => {
  const source = fileURLToPath(new URL("../../../", import.meta.url));
  const fixture = mkdtempSync(join(tmpdir(), "zpdf-fingerprint-"));
  try {
    copyFileSync(join(source, "Cargo.toml"), join(fixture, "Cargo.toml"));
    for (const entry of readdirSync(join(source, "crates"), { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      const dir = join("crates", entry.name);
      mkdirSync(join(fixture, dir, "src"), { recursive: true });
      copyFileSync(join(source, dir, "Cargo.toml"), join(fixture, dir, "Cargo.toml"));
    }
    const data = join(fixture, "crates/zpdf-font/data/adobe-cmaps");
    mkdirSync(data, { recursive: true });
    writeFileSync(join(data, "maps.bin"), new Uint8Array([0, 1, 2]));
    writeFileSync(join(data, "index.rs"), "const INDEX: u32 = 1;\n");
    const before = sourceFingerprint(fixture);
    writeFileSync(join(data, "maps.bin"), new Uint8Array([0, 1, 3]));
    const changedBinary = sourceFingerprint(fixture);
    assert.notEqual(changedBinary.source_sha256, before.source_sha256);
    writeFileSync(join(data, "index.rs"), "const INDEX: u32 = 2;\n");
    assert.notEqual(sourceFingerprint(fixture).source_sha256, changedBinary.source_sha256);
  } finally {
    rmSync(fixture, { recursive: true, force: true });
  }
});
