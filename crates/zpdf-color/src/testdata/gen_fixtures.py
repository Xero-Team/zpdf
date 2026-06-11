# Generates the .icc test fixtures in this directory.
#
#   uv run --with Pillow python gen_fixtures.py
#
# srgb.icc          - the sRGB IEC61966-2.1 profile as built by littlecms
#                     (via Pillow), i.e. a real-world RGB matrix/TRC profile.
# gray_gamma22.icc  - minimal hand-built ICC v2 gray profile with a gamma-2.2
#                     kTRC tone curve.
# gray_linear.icc   - same but gamma 1.0; midtones brighten visibly when
#                     converted to sRGB, unlike the near-identity gamma-2.2.
# cmyk_lut.icc      - minimal hand-built ICC v2 CMYK -> Lab lut8 (mft1) input
#                     profile with a 2-point grid encoding the naive
#                     (1-c)(1-k) CMYK->RGB model.

import struct

D50 = (0.9642, 1.0, 0.8249)


def s15f16(v):
    return struct.pack(">i", int(round(v * 65536.0)))


def tag_table(tags):
    """tags: list of (sig, bytes). Returns (table, body) with 128-byte-header-relative offsets."""
    table = struct.pack(">I", len(tags))
    offset = 128 + 4 + 12 * len(tags)
    body = b""
    for sig, data in tags:
        # ICC offsets/sizes are unpadded in the table; data is 4-byte aligned.
        table += sig + struct.pack(">II", offset, len(data))
        pad = (-len(data)) % 4
        body += data + b"\0" * pad
        offset += len(data) + pad
    return table, body


def header(size, dev_class, color_space, pcs):
    h = struct.pack(">I", size)
    h += b"none"                                  # CMM
    h += struct.pack(">I", 0x02400000)            # version 2.4
    h += dev_class + color_space + pcs
    h += struct.pack(">12H", 2024, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0)[:12]  # date
    h += b"acsp" + b"\0\0\0\0"                    # signature, platform
    h += struct.pack(">I", 0)                     # flags
    h += b"\0" * 8                                # manufacturer, model
    h += b"\0" * 8                                # attributes
    assert len(h) == 64, len(h)
    h += struct.pack(">I", 1)                     # rendering intent: relative
    h += s15f16(D50[0]) + s15f16(D50[1]) + s15f16(D50[2])  # illuminant D50
    h += b"none"                                  # creator
    h += b"\0" * 44                               # id + reserved
    assert len(h) == 128, len(h)
    return h


def desc_tag(text):
    s = text.encode() + b"\0"
    return b"desc" + b"\0" * 4 + struct.pack(">I", len(s)) + s + b"\0" * 78


def xyz_tag(xyz):
    return b"XYZ " + b"\0" * 4 + s15f16(xyz[0]) + s15f16(xyz[1]) + s15f16(xyz[2])


def curv_gamma(gamma):
    # Single-entry curv = gamma as u8.8 fixed point.
    return b"curv" + b"\0" * 4 + struct.pack(">IH", 1, int(round(gamma * 256.0)))


def text_tag(text):
    return b"text" + b"\0" * 4 + text.encode() + b"\0"


def build_profile(dev_class, color_space, pcs, tags):
    table, body = tag_table(tags)
    size = 128 + len(table) + len(body)
    return header(size, dev_class, color_space, pcs) + table + body


def gray_profile(gamma):
    tags = [
        (b"desc", desc_tag(f"zpdf test gray gamma {gamma}")),
        (b"wtpt", xyz_tag(D50)),
        (b"kTRC", curv_gamma(gamma)),
        (b"cprt", text_tag("zpdf test fixture, public domain")),
    ]
    return build_profile(b"mntr", b"GRAY", b"XYZ ", tags)


# --- CMYK lut8 ----------------------------------------------------------

def srgb_to_lab(r, g, b):
    """Naive sRGB (D65 matrix, no adaptation) -> Lab vs D50; test-grade math."""
    def lin(c):
        return c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4
    rl, gl, bl = lin(r), lin(g), lin(b)
    x = 0.4124 * rl + 0.3576 * gl + 0.1805 * bl
    y = 0.2126 * rl + 0.7152 * gl + 0.0722 * bl
    z = 0.0193 * rl + 0.1192 * gl + 0.9505 * bl
    def f(t):
        return t ** (1 / 3) if t > 0.008856 else 7.787 * t + 16 / 116
    fx, fy, fz = f(x / D50[0]), f(y / D50[1]), f(z / D50[2])
    return 116 * fy - 16, 500 * (fx - fy), 200 * (fy - fz)


def lab_to_lab8(l, a, b):
    """ICC v2 8-bit Lab encoding for lut8 PCS."""
    return (
        max(0, min(255, int(round(l * 255.0 / 100.0)))),
        max(0, min(255, int(round(a + 128.0)))),
        max(0, min(255, int(round(b + 128.0)))),
    )


def mft1_cmyk_to_lab():
    grid = 2
    data = b"mft1" + b"\0" * 4
    data += struct.pack(">BBBB", 4, 3, grid, 0)  # in ch, out ch, grid points, pad
    for v in (1, 0, 0, 0, 1, 0, 0, 0, 1):        # identity matrix
        data += s15f16(v)
    ramp = bytes(range(256))
    data += ramp * 4                              # input tables
    clut = b""
    for c in (0.0, 1.0):
        for m in (0.0, 1.0):
            for y in (0.0, 1.0):
                for k in (0.0, 1.0):
                    r = (1.0 - c) * (1.0 - k)
                    g = (1.0 - m) * (1.0 - k)
                    bb = (1.0 - y) * (1.0 - k)
                    clut += bytes(lab_to_lab8(*srgb_to_lab(r, g, bb)))
    data += clut
    data += ramp * 3                              # output tables
    return data


def cmyk_profile():
    tags = [
        (b"desc", desc_tag("zpdf test cmyk lut")),
        (b"wtpt", xyz_tag(D50)),
        (b"A2B0", mft1_cmyk_to_lab()),
        (b"cprt", text_tag("zpdf test fixture, public domain")),
    ]
    return build_profile(b"prtr", b"CMYK", b"Lab ", tags)


def srgb_profile():
    from PIL import ImageCms
    return ImageCms.ImageCmsProfile(ImageCms.createProfile("sRGB")).tobytes()


if __name__ == "__main__":
    import pathlib
    here = pathlib.Path(__file__).parent
    (here / "srgb.icc").write_bytes(srgb_profile())
    (here / "gray_gamma22.icc").write_bytes(gray_profile(2.2))
    (here / "gray_linear.icc").write_bytes(gray_profile(1.0))
    (here / "cmyk_lut.icc").write_bytes(cmyk_profile())
    for f in ("srgb.icc", "gray_gamma22.icc", "gray_linear.icc", "cmyk_lut.icc"):
        print(f, (here / f).stat().st_size, "bytes")
