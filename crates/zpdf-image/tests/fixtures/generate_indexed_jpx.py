"""Generate tiny synthetic palette JP2s (requires Pillow with OpenJPEG).

Run: uv run --with Pillow generate_indexed_jpx.py
Pillow encodes 8-bit grayscale. With no wavelet decomposition, encode the
same signed coefficients as the target precision, then change SIZ precision.
The explicit QCD exponents and coefficients stay unchanged; only the unsigned
level shift changes. JP2 boxes declare the matching precision and RGB palette.
"""

from io import BytesIO
from pathlib import Path
import struct

from PIL import Image

OUT = Path(__file__).parent


def box(kind, payload):
    return struct.pack(">I4s", len(payload) + 8, kind) + payload


for bits in (2, 4, 8):
    count = 1 << bits
    samples = bytes(128 - count // 2 + i for i in range(count))
    image = Image.frombytes("L", (count, 1), samples)
    buffer = BytesIO()
    image.save(buffer, format="JPEG2000", no_jp2=True,
               num_resolutions=1, irreversible=False)
    codestream = bytearray(buffer.getvalue())
    assert codestream[:4] == b"\xff\x4f\xff\x51" and codestream[42] == 7
    codestream[42] = bits - 1
    # Deliberately different from the PDF palette used by the regression.
    palette = bytes(channel for i in range(count) for channel in (i, i, i))
    header = box(b"ihdr", struct.pack(">IIHBBBB", 1, count, 1, bits - 1, 7, 0, 0))
    header += box(b"colr", bytes([1, 0, 0]) + struct.pack(">I", 16))
    header += box(b"pclr", struct.pack(">HB", count, 3) + bytes([7, 7, 7]) + palette)
    header += box(b"cmap", b"".join(struct.pack(">HBB", 0, 1, c) for c in range(3)))
    data = box(b"jP  ", b"\x0d\x0a\x87\x0a")
    data += box(b"ftyp", b"jp2 \x00\x00\x00\x00jp2 ")
    data += box(b"jp2h", header) + box(b"jp2c", codestream)
    path = OUT / f"indexed_{bits}bit.jp2"
    path.write_bytes(data)
    print(f"{path.name}: {len(data)} bytes")
