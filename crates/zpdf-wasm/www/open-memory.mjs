// Same harness for both packages. Run separately in an isolated environment:
// node --expose-gc open-memory.mjs /absolute/pkg reader|editor large|small 8
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { performance } from "node:perf_hooks";

const [directory, mode, size, count] = process.argv.slice(2);
assert.ok(["reader", "editor"].includes(mode));
assert.ok(["large", "small"].includes(size));
const iterations = Number(count);
assert.ok(Number.isInteger(iterations) && iterations > 0);
const bindings = await import(pathToFileURL(resolve(directory, "zpdf_wasm.js")));
const wasm = await bindings.default({ module_or_path: readFileSync(resolve(directory, "zpdf_wasm_bg.wasm")) });
const payload = size === "large" ? " ".repeat(16 * 1024 * 1024) : " ";
const content = "0 0 1 rg 10 10 20 20 re f";
const objects = [
  "<< /Type /Catalog /Pages 2 0 R >>",
  "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
  "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Contents 4 0 R >>",
  `<< /Length ${content.length} >>\nstream\n${content}\nendstream`,
  `<< /Length ${payload.length} >>\nstream\n${payload}\nendstream`,
];
let source = "%PDF-1.7\n";
const offsets = [];
for (const [i, body] of objects.entries()) {
  offsets.push(Buffer.byteLength(source));
  source += `${i + 1} 0 obj\n${body}\nendobj\n`;
}
const xref = Buffer.byteLength(source);
source += `xref\n0 6\n0000000000 65535 f \n${offsets.map(n => `${String(n).padStart(10, "0")} 00000 n \n`).join("")}`;
source += `trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
const input = Buffer.from(source);
const open = () => mode === "reader" ? bindings.Pdf.open(input) : bindings.PdfEditor.open(input);
const pages = () => wasm.memory.buffer.byteLength / 65536;
globalThis.gc?.();
const before = pages();
const first = open();
const after = pages();
const rssPeakKib = process.resourceUsage().maxRSS;
assert.equal(first.page_count, 1);
const verification = mode === "editor" ? bindings.Pdf.open(first.save()) : first;
const bitmap = verification.render_page(0, 1);
const pixels = bitmap.into_rgba();
const outputHash = createHash("sha256").update(pixels).digest("hex");
if (mode === "editor") verification.free();
first.free();
const samples = [];
for (let sample = 0; sample < 11; sample++) {
  globalThis.gc?.();
  const start = performance.now();
  for (let i = 0; i < iterations; i++) open().free();
  samples.push((performance.now() - start) * 1e6 / iterations);
}
console.log(JSON.stringify({ case: `${mode}-${size}`, iterations,
  input_sha256: createHash("sha256").update(input).digest("hex"),
  output_sha256: outputHash, input_bytes: input.length,
  first_open_pages: { before, after, growth: after - before }, rss_peak_kib: rssPeakKib,
  samples_ns: samples }));
