#!/usr/bin/env python3
"""Generate tiny deterministic single-feature PDFs for the GPU<->CPU compare harness.

Each PDF is hand-built with a correct classic xref table so zpdf's parser (no
tail-scan fallback yet) opens it cleanly. Run: `uv run python tests/gen_corpus.py`.
"""
import os
import struct  # noqa: F401 (kept for future binary streams)

OUT = os.path.join(os.path.dirname(__file__), "corpus")
os.makedirs(OUT, exist_ok=True)


def assemble(objs: list) -> bytes:
    """Concatenate 1-based objects and append a classic xref table + trailer."""
    out = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
    offsets = [0]  # object 0 is the free head
    for i, body in enumerate(objs, start=1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % i + body + b"\nendobj\n"
    xref_pos = len(out)
    n = len(objs) + 1
    out += b"xref\n0 %d\n" % n + b"0000000000 65535 f \n"
    for off in offsets[1:]:
        out += b"%010d 00000 n \n" % off
    out += (b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n"
            % (n, xref_pos))
    return bytes(out)


def build_pdf(content: bytes, media=(0, 0, 200, 200)) -> bytes:
    """A 4-object PDF (catalog, pages, page, content) with an empty resource dict."""
    mb = " ".join(str(v) for v in media)
    return assemble([
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        ("<< /Type /Page /Parent 2 0 R /MediaBox [%s] "
         "/Contents 4 0 R /Resources << >> >>" % mb).encode(),
        b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream",
    ])


def write(name: str, content: bytes, media=(0, 0, 200, 200)):
    path = os.path.join(OUT, name)
    with open(path, "wb") as f:
        f.write(build_pdf(content, media))
    print("wrote", path)


# rect_fills: nonzero + even-odd fills, multiple device colors.
write("rect_fills.pdf", b"""\
0 0 1 rg 20 20 80 80 re f
0 1 0 rg 110 20 70 70 re f
1 0 0 rg
40 120 m 100 190 l 160 120 l h f
0 0 0 rg
30 140 140 40 re
60 150 80 20 re
f*
""")

# strokes: caps, joins, widths.
write("strokes.pdf", b"""\
0 0 0 RG
8 w 1 J 1 j
20 30 m 100 170 l 180 30 l S
2 w 0 J 0 j
1 0 0 RG
20 100 m 180 100 l S
14 w 2 J
0 0 1 RG
40 60 m 160 60 l S
""")

# curves: cubic bezier fills + thin stroked curve.
write("curves.pdf", b"""\
0.2 0.4 0.8 rg
30 100 m
30 170 90 170 100 100 c
110 30 170 30 170 100 c
f
0 0 0 RG 1 w
20 50 m 60 10 140 190 180 150 c S
""")

# clip: nested rectangular clips (intersection) + rebuild-on-pop.
write("clip.pdf", b"""\
1 1 0 rg 0 0 200 200 re f
q
30 30 140 140 re W n
0 0 1 rg 0 0 200 200 re f
q
60 60 120 60 re W n
1 0 0 rg 0 0 200 200 re f
Q
0 1 0 rg 0 0 200 45 re f
Q
0 0 0 rg 175 175 20 20 re f
""")


def inline_img(w, h, rgb):
    """An inline image operator (BI/ID/EI) with raw 8-bit RGB samples."""
    return (b"BI /W %d /H %d /CS /RGB /BPC 8 ID " % (w, h)) + rgb + b" EI"


# image_rgb: 3 inline RGB images (>=2 gates the multi-image arena), incl. one with
# a Y-flipped CTM (d < 0); it must land in the lower half, vertically mirrored per
# its own matrix (exercises the unified image-placement affine).
_IMG_A = bytes([255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0])      # R G B Y
_IMG_B = bytes([0, 255, 255, 255, 0, 255, 255, 255, 255, 0, 0, 0])  # C M W K
write("image_rgb.pdf",
      b"q 70 0 0 70 20 110 cm " + inline_img(2, 2, _IMG_A) + b" Q\n"
      b"q 70 0 0 70 110 110 cm " + inline_img(2, 2, _IMG_B) + b" Q\n"
      b"q 80 0 0 -80 60 90 cm " + inline_img(2, 2, _IMG_A) + b" Q\n")

# image_under_clip: a page-covering image clipped to a centered rect (the image
# must honor the clip stencil).
write("image_under_clip.pdf",
      b"1 1 0 rg 0 0 200 200 re f\n"
      b"q 50 50 100 100 re W n\n"
      b"q 200 0 0 200 0 0 cm " + inline_img(2, 2, _IMG_B) + b" Q\n"
      b"Q\n")


def write_type3():
    """A Type3 font whose single glyph 'sq' (code 65) is a filled square; the page
    paints "AAA". Exercises the Type3 char-proc path (no embedded outline font)."""
    glyph = b"1000 0 d0\n150 150 700 700 re\nf"
    content = b"0 0 0 rg\nBT /F1 60 Tf 15 70 Td (AAA) Tj ET"
    objs = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] "
        b"/Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>",
        b"<< /Length %d >>\nstream\n" % len(content) + content + b"\nendstream",
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] "
        b"/FontMatrix [0.001 0 0 0.001 0 0] /CharProcs 6 0 R /Encoding 7 0 R "
        b"/FirstChar 65 /LastChar 65 /Widths [1000] /Resources << >> >>",
        b"<< /sq 8 0 R >>",
        b"<< /Type /Encoding /Differences [65 /sq] >>",
        b"<< /Length %d >>\nstream\n" % len(glyph) + glyph + b"\nendstream",
    ]
    path = os.path.join(OUT, "text_type3.pdf")
    with open(path, "wb") as f:
        f.write(assemble(objs))
    print("wrote", path)


write_type3()


def stream_obj(dict_body: bytes, content: bytes) -> bytes:
    return (b"<< " + dict_body + b" /Length %d >>\nstream\n" % len(content)
            + content + b"\nendstream")


def write_tiling():
    """Tiling patterns: a colored cell (PaintType 1) tiled over a rect and a
    triangle (non-rect clip), plus a 45-degree /Matrix variant."""
    cell = (b"1 0 0 rg 2 2 7 7 re f\n"          # red square
            b"0 0 1 rg 11 11 7 7 re f\n"        # blue square
            b"0 0.6 0 RG 1 w 0 20 m 20 0 l S")  # green diagonal hatch
    pat = (b"/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 "
           b"/BBox [0 0 20 20] /XStep 20 /YStep 20 /Resources << >>")
    # ~45-degree rotation, same cell.
    pat_rot = (b"/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 "
               b"/BBox [0 0 20 20] /XStep 20 /YStep 20 /Resources << >> "
               b"/Matrix [0.7071 0.7071 -0.7071 0.7071 0 0]")
    content = (b"/Pattern cs /P0 scn 10 110 80 80 re f\n"
               b"/P1 scn 110 110 80 80 re f\n"
               b"/P0 scn 20 10 m 100 90 l 180 10 l h f\n")
    objs = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] "
        b"/Contents 4 0 R /Resources << /Pattern << /P0 5 0 R /P1 6 0 R >> >> >>",
        stream_obj(b"", content),
        stream_obj(pat, cell),
        stream_obj(pat_rot, cell),
    ]
    path = os.path.join(OUT, "pattern_tiling.pdf")
    with open(path, "wb") as f:
        f.write(assemble(objs))
    print("wrote", path)


def write_tiling_uncolored():
    """Uncolored tiling pattern (PaintType 2): one cell painted with two
    different scn colors via [/Pattern /DeviceRGB]; cell color ops are absent
    by definition."""
    cell = b"2 2 7 7 re f\n11 11 7 7 re f"
    pat = (b"/Type /Pattern /PatternType 1 /PaintType 2 /TilingType 1 "
           b"/BBox [0 0 20 20] /XStep 20 /YStep 20 /Resources << >>")
    content = (b"/CS0 cs\n"
               b"1 0 0 /P0 scn 10 10 85 180 re f\n"
               b"0 0 1 /P0 scn 105 10 85 180 re f\n")
    objs = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] "
        b"/Contents 4 0 R /Resources << /Pattern << /P0 5 0 R >> "
        b"/ColorSpace << /CS0 6 0 R >> >> >>",
        stream_obj(b"", content),
        stream_obj(pat, cell),
        b"[/Pattern /DeviceRGB]",
    ]
    path = os.path.join(OUT, "pattern_tiling_uncolored.pdf")
    with open(path, "wb") as f:
        f.write(assemble(objs))
    print("wrote", path)


write_tiling()
write_tiling_uncolored()

print("done")
