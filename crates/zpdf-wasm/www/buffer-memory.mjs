// Compare the checked-in baseline and regenerated package in separate Node
// processes: node --expose-gc buffer-memory.mjs /absolute/path/to/pkg copy|move
// Rendering is excluded from transfer timing. Pixel hashes must match.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { performance } from "node:perf_hooks";

const directory = resolve(process.argv[2]);
const mode = process.argv[3];
assert.ok(mode === "copy" || mode === "move");
const bindings = await import(pathToFileURL(resolve(directory, "zpdf_wasm.js")));
const wasm = await bindings.default({ module_or_path: readFileSync(resolve(directory, "zpdf_wasm_bg.wasm")) });
const content = "0 0 1 rg 0 0 3840 2160 re f";
const objects = [
  "<< /Type /Catalog /Pages 2 0 R >>",
  "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
  "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 3840 2160] /Contents 4 0 R >>",
  `<< /Length ${content.length} >>\nstream\n${content}\nendstream`,
];
let input = "%PDF-1.7\n";
const offsets = [];
for (const [index, body] of objects.entries()) {
  offsets.push(Buffer.byteLength(input));
  input += `${index + 1} 0 obj\n${body}\nendobj\n`;
}
const xref = Buffer.byteLength(input);
input += `xref\n0 5\n0000000000 65535 f \n${offsets.map(offset => `${String(offset).padStart(10, "0")} 00000 n \n`).join("")}`;
input += `trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
const pdf = bindings.Pdf.open(Buffer.from(input));
const samples = [];
let expectedHash, firstMemory;
try {
  for (let index = 0; index < 12; index++) {
    globalThis.gc?.();
    const bitmap = pdf.render_page(0, 1);
    assert.equal(bitmap.width, 3840);
    assert.equal(bitmap.height, 2160);
    const before = wasm.memory.buffer.byteLength;
    const start = performance.now();
    const pixels = mode === "move" ? bitmap.into_rgba() : bitmap.rgba();
    const elapsed = performance.now() - start;
    const after = wasm.memory.buffer.byteLength;
    if (mode === "copy") bitmap.free();
    assert.equal(pixels.length, 3840 * 2160 * 4);
    const hash = createHash("sha256").update(pixels).digest("hex");
    expectedHash ??= hash;
    assert.equal(hash, expectedHash);
    if (index === 0) firstMemory = { before, after, growth: after - before };
    else samples.push(elapsed);
  }
} finally { pdf.free(); }
console.log(JSON.stringify({ mode, output_sha256: expectedHash, output_bytes: 3840 * 2160 * 4, first_transfer_wasm_memory: firstMemory, samples_ms: samples }));
