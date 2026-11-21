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

test("packaged reader/editor own input after the caller reuses its buffer", () => {
  for (const passwordEntry of [false, true]) {
    const input = fixture();
    const pdf = passwordEntry ? bindings.Pdf.open_with_password(input, "") : bindings.Pdf.open(input);
    const editor = passwordEntry ? bindings.PdfEditor.open_with_password(input, "") : bindings.PdfEditor.open(input);
    input.fill(0);
    try {
      assert.match(pdf.page_text(0), /HELLO/);
      editor.rotate_page(0, 90);
      const reopened = bindings.Pdf.open(editor.save());
      try {
        assert.deepEqual(Array.from(reopened.page_size(0)), [80, 120]);
        assert.match(reopened.page_text(0), /HELLO/);
      } finally { reopened.free(); }
    } finally { editor.free(); pdf.free(); }
  }
});

test("packaged bitmap transfers pixels while the copy API stays independent", () => {
  const pdf = bindings.Pdf.open(fixture());
  try {
    const bitmap = pdf.render_page(0, 1);
    const width = bitmap.width, height = bitmap.height;
    const expected = bitmap.rgba();
    const copy = bitmap.rgba();
    copy.fill(0);
    const transferred = bitmap.into_rgba(); // consumes bitmap; no free() afterwards
    assert.equal(transferred.length, width * height * 4);
    assert.deepEqual(transferred, expected);
    // JS owns the returned bytes after the Rust bitmap has been consumed and
    // after a later render allocates and releases WASM memory.
    const next = pdf.render_page(1, 2);
    next.free();
    assert.deepEqual(transferred, expected);
  } finally { pdf.free(); }
});

test("packaged reader rejects truncated JPX before exporting", () => {
  const codestream = readFileSync(new URL("../../zpdf-image/tests/fixtures/gray.j2k", import.meta.url));
  const content = "q 32 0 0 24 0 0 cm /Im0 Do Q";
  for (const length of [codestream.length, Math.floor(codestream.length / 2)]) {
    const objects = [
      "<< /Type /Catalog /Pages 2 0 R >>",
      "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
      "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 32 24] /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>",
      Buffer.concat([
        Buffer.from(`<< /Type /XObject /Subtype /Image /Width 32 /Height 24 /Filter /JPXDecode /Length ${length} >>\nstream\n`),
        codestream.subarray(0, length), Buffer.from("\nendstream"),
      ]),
      `<< /Length ${content.length} >>\nstream\n${content}\nendstream`,
    ];
    const chunks = [Buffer.from("%PDF-1.7\n")];
    const offsets = [];
    let size = chunks[0].length;
    for (const [i, body] of objects.entries()) {
      offsets.push(size);
      const object = Buffer.concat([Buffer.from(`${i + 1} 0 obj\n`), Buffer.from(body), Buffer.from("\nendobj\n")]);
      chunks.push(object);
      size += object.length;
    }
    const xref = offsets.map(offset => `${String(offset).padStart(10, "0")} 00000 n \n`).join("");
    chunks.push(Buffer.from(`xref\n0 6\n0000000000 65535 f \n${xref}trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n${size}\n%%EOF\n`));
    const pdf = bindings.Pdf.open(Buffer.concat(chunks));
    try {
      if (length === codestream.length) {
        assert.match(pdf.page_svg(0), /<svg/);
        const bitmap = pdf.render_page(0, 1);
        try { assert.deepEqual([bitmap.width, bitmap.height], [32, 24]); }
        finally { bitmap.free(); }
      } else {
        assert.throws(() => pdf.page_svg(0), /incomplete output.*decode_failures=1/);
        assert.throws(() => pdf.render_page(0, 1), /incomplete output.*decode_failures=1/);
      }
    } finally { pdf.free(); }
  }
});

test("packaged reader recovers corrupt embedded font with a substitute and rejects missing output", () => {
  const content = "0 0 0 rg BT /F1 24 Tf 15 40 Td (ABC) Tj ET";
  const corruptFont = "not a valid truetype program";
  const objects = [
    "<< /Type /Catalog /Pages 2 0 R >>",
    "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
    `<< /Length ${content.length} >>\nstream\n${content}\nendstream`,
    "<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+BrokenTTF /Encoding /WinAnsiEncoding /FirstChar 65 /LastChar 67 /Widths [620 780 540] /FontDescriptor 6 0 R >>",
    "<< /Type /FontDescriptor /FontName /ABCDEF+BrokenTTF /Flags 32 /FontFile2 7 0 R >>",
    `<< /Length ${corruptFont.length} >>\nstream\n${corruptFont}\nendstream`,
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
  const pdf = bindings.Pdf.open(Buffer.from(out));
  try {
    // Without a substitute font, raster/SVG reject missing ink while text extraction
    // still returns the cleanly decoded characters.
    assert.throws(() => pdf.render_page(0, 1), /incomplete output.*embedded_font_fallbacks=1.*missing_glyph_outlines=1/);
    assert.throws(() => pdf.page_svg(0), /incomplete output.*embedded_font_fallbacks=1.*missing_glyph_outlines=1/);
    assert.match(pdf.page_text(0), /ABC/);

    // Registering a substitute recovers every glyph -> render_page, page_svg, and page_text succeed.
    pdf.register_font(font, 0, "BrokenTTF");
    const bitmap = pdf.render_page(0, 1);
    try {
      const pixels = bitmap.rgba();
      let dark = 0;
      for (let i = 0; i < pixels.length; i += 4) {
        if (pixels[i] < 200 || pixels[i + 1] < 200 || pixels[i + 2] < 200) dark++;
      }
      assert.ok(dark > 50, `expected rendered fallback ink, got ${dark} dark pixels`);
    } finally { bitmap.free(); }
    const svg = pdf.page_svg(0);
    assert.match(svg, /<svg/);
    assert.match(svg, /<path/);
    assert.match(pdf.page_text(0), /ABC/);
  } finally { pdf.free(); }

  // Tagged PDF whose parent structure element supplies /ActualText (Label) over
  // an unmapped fallback CID: page_text(0) returns "Label" while page_svg(0)
  // still rejects missing ink; an uncovered unmapped MCID fails page_text(0).
  const assembleTagged = (streamBody, extraKid) => {
    const kids = extraKid ? "[10 0 R 11 0 R]" : "[10 0 R]";
    const objs = [
      "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 9 0 R /MarkInfo << /Marked true >> >>",
      "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
      "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
      `<< /Length ${streamBody.length} >>\nstream\n${streamBody}\nendstream`,
      "<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+TaggedCID /Encoding /Identity-H /DescendantFonts [6 0 R] >>",
      "<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+TaggedCID /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor 7 0 R /DW 1000 >>",
      "<< /Type /FontDescriptor /FontName /ABCDEF+TaggedCID /Flags 32 /FontFile2 8 0 R >>",
      `<< /Length ${corruptFont.length} >>\nstream\n${corruptFont}\nendstream`,
      `<< /Type /StructTreeRoot /K ${kids} >>`,
      "<< /Type /StructElem /S /Span /Pg 3 0 R /ActualText (Label) /K 0 >>",
    ];
    if (extraKid) objs.push("<< /Type /StructElem /S /Span /Pg 3 0 R /K 1 >>");
    let raw = "%PDF-1.7\n";
    const offs = [];
    for (const [i, body] of objs.entries()) {
      offs.push(Buffer.byteLength(raw));
      raw += `${i + 1} 0 obj\n${body}\nendobj\n`;
    }
    const xrefOff = Buffer.byteLength(raw);
    raw += `xref\n0 ${objs.length + 1}\n0000000000 65535 f \n`;
    for (const off of offs) raw += `${String(off).padStart(10, "0")} 00000 n \n`;
    raw += `trailer\n<< /Size ${objs.length + 1} /Root 1 0 R >>\nstartxref\n${xrefOff}\n%%EOF\n`;
    return Buffer.from(raw);
  };
  const taggedCovered = bindings.Pdf.open(
    assembleTagged("0 0 0 rg /Span << /MCID 0 >> BDC BT /F1 24 Tf 15 40 Td <0001> Tj ET EMC", false),
  );
  try {
    assert.equal(taggedCovered.page_text(0).trim(), "Label");
    assert.throws(
      () => taggedCovered.page_svg(0),
      /incomplete output.*embedded_font_fallbacks=1.*missing_glyph_outlines=1/,
    );
  } finally { taggedCovered.free(); }
  const taggedUncovered = bindings.Pdf.open(
    assembleTagged(
      "0 0 0 rg /Span << /MCID 0 >> BDC BT /F1 24 Tf 15 40 Td <0001> Tj ET EMC /Span << /MCID 1 >> BDC BT /F1 24 Tf 60 40 Td <0002> Tj ET EMC",
      true,
    ),
  );
  try {
    assert.throws(
      () => taggedUncovered.page_text(0),
      /incomplete output.*decode_failures=1.*embedded_font_fallbacks=1/,
    );
  } finally { taggedUncovered.free(); }
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

test("packaged redaction saves a fresh document and preserves later edits", () => {
  const input = fixture();
  const editor = bindings.PdfEditor.open(input);
  try {
    editor.set_title("Before redaction");
    editor.redact(0, new Float64Array([0, 0, 120, 80]), null);
    editor.rotate_page(1, 90);
    for (const title of ["Redacted", "Saved again"]) {
      editor.set_title(title);
      const saved = Buffer.from(editor.save());
      assert.equal(saved.includes(input), false, "original revision must not survive");
      assert.equal(saved.includes(Buffer.from("(HELLO) Tj")), false);
      const pdf = bindings.Pdf.open(saved);
      try {
        assert.equal(pdf.page_count, 3);
        assert.equal(pdf.page_text(0).trim(), "");
        assert.equal(pdf.title, title);
        assert.deepEqual(Array.from(pdf.page_size(1)), [100, 200]);
      } finally { pdf.free(); }
    }
  } finally { editor.free(); }
});
