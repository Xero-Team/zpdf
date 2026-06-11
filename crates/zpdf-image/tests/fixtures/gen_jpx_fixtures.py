# Regenerates the JPEG 2000 fixtures in this directory.
#
#   uv run --with Pillow gen_jpx_fixtures.py
#
# Pillow's bundled OpenJPEG is used only at fixture-generation time; the
# decoder under test (hayro-jpeg2000) is pure Rust. Lossless (reversible 5/3)
# fixtures come with byte-exact .raw references; the lossy (irreversible 9/7)
# fixture is checked with a tolerance.
import pathlib

from PIL import Image

OUT = pathlib.Path(__file__).parent
W, H = 32, 24


def rgb_pattern():
    img = Image.new("RGB", (W, H))
    px = img.load()
    for y in range(H):
        for x in range(W):
            # Smooth gradients plus hard quadrant edges.
            r = (x * 255) // (W - 1)
            g = (y * 255) // (H - 1)
            b = 255 if (x >= W // 2) != (y >= H // 2) else 0
            px[x, y] = (r, g, b)
    return img


def gray_pattern():
    img = Image.new("L", (W, H))
    px = img.load()
    for y in range(H):
        for x in range(W):
            px[x, y] = (x * 8 + y * 3) % 256
    return img


def rgba_pattern():
    img = rgb_pattern().convert("RGBA")
    px = img.load()
    for y in range(H):
        for x in range(W):
            r, g, b, _ = px[x, y]
            px[x, y] = (r, g, b, (x * 255) // (W - 1))
    return img


rgb = rgb_pattern()
rgb.save(OUT / "rgb.jp2", irreversible=False)
(OUT / "rgb_ref.raw").write_bytes(rgb.tobytes())

gray = gray_pattern()
gray.save(OUT / "gray.j2k", irreversible=False)
(OUT / "gray_ref.raw").write_bytes(gray.tobytes())

rgba = rgba_pattern()
rgba.save(OUT / "rgba.jp2", irreversible=False)
(OUT / "rgba_ref.raw").write_bytes(rgba.tobytes())

# Keep the lossy rate mild (2:1): heavily truncated codestreams leave the
# reconstruction of missing coefficient bits decoder-defined, which makes
# codec-to-codec comparisons meaningless at aggressive rates.
rgb.save(OUT / "rgb_lossy.jp2", irreversible=True, quality_mode="rates", quality_layers=[2])
# The lossy reference is OpenJPEG's own decode of the lossy file (the 9/7
# wavelet is not exactly invertible), so the Rust decoder is compared
# codec-to-codec with a small tolerance.
lossy_ref = Image.open(OUT / "rgb_lossy.jp2").convert("RGB")
(OUT / "rgb_lossy_ref.raw").write_bytes(lossy_ref.tobytes())

for name in ("rgb.jp2", "gray.j2k", "rgba.jp2", "rgb_lossy.jp2"):
    data = (OUT / name).read_bytes()
    print(f"{name}: {len(data)} bytes, magic {data[:12].hex()}")
