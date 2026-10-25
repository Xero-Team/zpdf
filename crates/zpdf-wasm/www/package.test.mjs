// Exercise the checked-in JS and WASM together, without a native Rust fallback.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { deflateSync } from "node:zlib";
import * as bindings from "./pkg/zpdf_wasm.js";
import { sourceFingerprint, artifactFingerprints } from "./package-fingerprint.mjs";

await bindings.default({
  module_or_path: readFileSync(new URL("./pkg/zpdf_wasm_bg.wasm", import.meta.url)),
});
const font = readFileSync(new URL("../../zpdf-font/tests/fixtures/coverage-primary.ttf", import.meta.url));

test("packaged artifacts match the recorded source fingerprint", () => {
  const manifest = JSON.parse(readFileSync(new URL("./pkg/build.json", import.meta.url), "utf8"));
  const source = sourceFingerprint();
  assert.equal(manifest.source_sha256, source.source_sha256, "Rust sources changed: run build-web.sh and commit the regenerated package");
  assert.equal(manifest.source_files, source.source_files);
  assert.deepEqual(manifest.artifacts, artifactFingerprints());
});

function fixture() {
  const content = "0 0 1 rg 10 10 20 20 re f BT /F1 12 Tf 10 65 Td (HELLO) Tj ET";
  const objects = [
    "<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [8 0 R] /DA (/F1 12 Tf 0 g) /DR << /Font << /F1 6 0 R >> >> >> >>",
    "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>",
    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 120 80] /Contents 7 0 R /Annots [8 0 R] /Resources << /Font << /F1 6 0 R >> >> >>",
    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] >>",
    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] >>",
    "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    `<< /Length ${content.length} >>\nstream\n${content}\nendstream`,
    "<< /Type /Annot /Subtype /Widget /FT /Tx /T (name) /V (old) /Rect [10 35 90 60] /P 3 0 R >>",
  ];
  let out = "%PDF-1.7\n";
  const offsets = [];
  for (const [i, body] of objects.entries()) {
    offsets.push(Buffer.byteLength(out));
    out += `${i + 1} 0 obj\n${body}\nendobj\n`;
  }
  const xref = Buffer.byteLength(out);
  out += `xref\n0 ${objects.length + 1}\n0000000000 65535 f \n`;
  for (const offset of offsets) out += `${String(offset).padStart(10, "0")} 00000 n \n`;
  out += `trailer\n<< /Size ${objects.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return Buffer.from(out);
}

test("packaged exports include editing and font registration", () => {
  for (const name of ["Pdf", "PdfEditor", "PageBitmap", "OptimizeOptions", "optimize", "split_pages"]) {
    assert.equal(typeof bindings[name], "function", name);
  }
  for (const name of ["register_font", "register_zlib_font"]) {
    assert.equal(typeof bindings.Pdf.prototype[name], "function", `Pdf.${name}`);
  }
  assert.equal(typeof bindings.PdfEditor.prototype.register_font, "function");
});

test("packaged reader renders and loads raw/compressed fonts", () => {
  const pdf = bindings.Pdf.open(fixture());
  try {
    assert.equal(pdf.page_count, 3);
    assert.match(pdf.page_text(0), /HELLO/);
    assert.deepEqual(Array.from(pdf.page_size(0)), [120, 80]);
    pdf.register_font(font, 0, "Coverage");
    pdf.register_zlib_font(deflateSync(font), 0, "CompressedCoverage", font.length);
    const bitmap = pdf.render_page(0, 1);
    try {
      assert.equal(bitmap.width, 120);
      assert.equal(bitmap.height, 80);
      const offset = ((80 - 20) * 120 + 20) * 4;
      assert.deepEqual(Array.from(bitmap.rgba().slice(offset, offset + 4)), [0, 0, 255, 255]);
    } finally { bitmap.free(); }
    assert.match(pdf.page_svg(0), /<svg/);
  } finally { pdf.free(); }
});

test("packaged editor saves Unicode, page edits, extraction and optimization", () => {
  const editor = bindings.PdfEditor.open(fixture());
  let saved;
  try {
    editor.register_font(font, 0);
    editor.fill_fields(["name"], ["中"]);
    editor.delete_pages(new Uint32Array([1]));
    editor.reorder_pages(new Uint32Array([1, 0]));
    editor.rotate_page(1, 90);
    editor.stamp_text(0, "中", 20, 50, "Helvetica", 16, 0, 0, 0);
    editor.set_title("同步 • €");
    assert.equal(editor.page_count, 2);
    saved = editor.save();
  } finally { editor.free(); }
  const pdf = bindings.Pdf.open(saved);
  try {
    assert.equal(pdf.page_count, 2);
    assert.equal(pdf.title, "同步 • €");
    assert.deepEqual(Array.from(pdf.page_size(1)), [80, 120]);
    assert.match(pdf.page_text(0), /中/);
  } finally { pdf.free(); }
  const split = bindings.split_pages(saved, new Uint32Array([1]));
  const options = new bindings.OptimizeOptions();
  options.compress = true;
  const optimized = bindings.optimize(split, options); // consumes options
  const one = bindings.Pdf.open(optimized);
  try {
    assert.equal(one.page_count, 1);
    assert.deepEqual(Array.from(one.page_size(0)), [80, 120]);
  } finally { one.free(); }
});
