import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { extname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("../../../", import.meta.url));
const pkg = new URL("./pkg/", import.meta.url);
// Library crates linked into the browser package. GUI/GPU/bench crates do not
// participate. Include source assets as well as Rust and build manifests.
const crates = ["zpdf", "zpdf-core", "zpdf-parser", "zpdf-document", "zpdf-content",
  "zpdf-display-list", "zpdf-font", "zpdf-color", "zpdf-image", "zpdf-render",
  "zpdf-render-cpu", "zpdf-writer", "zpdf-svg-export", "zpdf-wasm"];
const sha256 = bytes => createHash("sha256").update(bytes).digest("hex");
const normalized = (path, bytes) => [".rs", ".toml", ".js"].includes(extname(path))
  ? Buffer.from(bytes.toString("utf8").replace(/\r\n/g, "\n")) : bytes;

export function sourceFingerprint(sourceRoot = root) {
  const files = ["Cargo.toml"];
  const walk = path => {
    for (const item of readdirSync(resolve(sourceRoot, path), { withFileTypes: true })) {
      const next = `${path}/${item.name}`;
      if (item.isDirectory()) walk(next);
      else if (item.isFile()) files.push(next);
    }
  };
  for (const crate of crates) {
    const dir = `crates/${crate}`;
    files.push(`${dir}/Cargo.toml`);
    if (existsSync(resolve(sourceRoot, dir, "build.rs"))) files.push(`${dir}/build.rs`);
    walk(`${dir}/src`);
    // include_bytes!/include! also link assets outside src, notably Adobe's
    // compiled CMaps. Their changes must invalidate the shipped WASM too.
    if (existsSync(resolve(sourceRoot, dir, "data"))) walk(`${dir}/data`);
  }
  if (existsSync(resolve(sourceRoot, ".cargo/config.toml"))) files.push(".cargo/config.toml");
  const hash = createHash("sha256");
  for (const path of files.sort()) {
    hash.update(path).update("\0");
    hash.update(sha256(normalized(path, readFileSync(resolve(sourceRoot, path))))).update("\n");
  }
  return { source_sha256: hash.digest("hex"), source_files: files.length };
}

export function artifactFingerprints() {
  return Object.fromEntries(["zpdf_wasm.js", "zpdf_wasm_bg.wasm"].map(name =>
    [name, sha256(normalized(name, readFileSync(new URL(name, pkg))))]));
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  if (process.argv[2] !== "--write") throw new Error("Use --write only after building and running wasm-bindgen");
  const manifest = {
    schema_version: 1,
    rustc: execFileSync("rustc", ["--version"], { encoding: "utf8" }).trim(),
    wasm_bindgen: execFileSync("wasm-bindgen", ["--version"], { encoding: "utf8" }).trim(),
    ...sourceFingerprint(),
    artifacts: artifactFingerprints(),
  };
  writeFileSync(new URL("build.json", pkg), `${JSON.stringify(manifest, null, 2)}\n`);
}
