#!/usr/bin/env python3
"""Generate original MIT-licensed test fonts without external dependencies.

These deliberately geometric outlines test coverage, glyph identity and PDF
placement. They are not typography-quality fallbacks or real-font performance
proxies. The benchmark also exercises the existing Quartz CFF fixture.
"""

import argparse
import struct
from pathlib import Path


def u16(value):
    return struct.pack(">H", value)


def u32(value):
    return struct.pack(">I", value)


def checksum(data):
    data += bytes((-len(data)) % 4)
    return sum(struct.unpack(">" + "I" * (len(data) // 4), data)) & 0xFFFFFFFF


def outline(index):
    width = 240 + (index % 17) * 23
    height = 420 + (index % 11) * 29
    header = struct.pack(">hhhhh", 1, 40, 0, width, height)
    # Four on-curve points, with signed 16-bit relative coordinates.
    return (header + u16(3) + u16(0) + bytes([1] * 4)
            + struct.pack(">hhhh", 40, width - 40, 0, 40 - width)
            + struct.pack(">hhhh", 0, 0, height, 0))


def make_font(family, codepoints, bold=False, variations=False, vertical=False, vorg=False,
              blank=frozenset({32})):
    codepoints = sorted(set(codepoints))
    count = len(codepoints) + 1
    glyf = bytearray()
    offsets = [0]
    for index, codepoint in enumerate([None] + codepoints):
        if codepoint not in blank:
            glyf.extend(outline(index))
            glyf.extend(bytes((-len(glyf)) % 4))
        offsets.append(len(glyf))

    head = bytearray(54)
    struct.pack_into(">IIIIHH", head, 0, 0x10000, 0x10000, 0, 0x5F0F3CF5, 0, 1000)
    struct.pack_into(">hhhhHHhhh", head, 36, 0, -200, 1000, 900, 0, 8, 2, 1, 0)
    if vertical:
        struct.pack_into(">H", head, 18, 2000)
    hhea = bytearray(36)
    struct.pack_into(">IhhhH", hhea, 0, 0x10000, 900, -200, 0, 1000)
    struct.pack_into(">h", hhea, 18, 1)
    struct.pack_into(">H", hhea, 34, count)
    maxp = u32(0x10000) + u16(count) + bytes(26)
    hmtx = b"".join(struct.pack(">Hh", 350 if cp == 32 else 1000, 40)
                    for cp in [None] + codepoints)
    if family == "ZpdfCoverageCMap":
        hmtx = b"".join(struct.pack(">Hh", (index + 3) * 97, 40) for index in range(count))

    groups = b"".join(struct.pack(">III", cp, cp, index + 1)
                      for index, cp in enumerate(codepoints))
    subtable = struct.pack(">HHIII", 12, 0, 16 + len(groups), 0, len(codepoints)) + groups
    cmap = struct.pack(">HHHHI", 0, 1, 3, 10, 12) + subtable
    if variations:
        # U+4E2D + FE00 uses the default glyph; E0100 selects U+4E2E's
        # deliberately different geometric glyph through cmap format 14.
        default = u32(1) + (0x4E2D).to_bytes(3, "big") + bytes([0])
        alternate = u32(1) + (0x4E2D).to_bytes(3, "big") + u16(codepoints.index(0x4E2E) + 1)
        records = ((0xFE00).to_bytes(3, "big") + u32(32) + u32(0)
                   + (0xE0100).to_bytes(3, "big") + u32(0) + u32(40))
        uvs = u16(14) + u32(49) + u32(2) + records + default + alternate
        cmap = (struct.pack(">HHHHIHHI", 0, 2, 3, 10, 20, 0, 5, 20 + len(subtable))
                + subtable + uvs)
    strings = bytearray()
    records = bytearray()
    for name_id, value in [(1, family), (2, "Regular"), (4, family), (6, family)]:
        encoded = value.encode("utf-16-be")
        records.extend(struct.pack(">HHHHHH", 3, 1, 0x409, name_id, len(encoded), len(strings)))
        strings.extend(encoded)
    name = struct.pack(">HHH", 0, 4, 54) + records + strings
    post = u32(0x30000) + bytes(28)
    tables = {b"head": bytes(head), b"hhea": bytes(hhea), b"maxp": maxp,
              b"hmtx": hmtx, b"glyf": bytes(glyf), b"loca": b"".join(map(u32, offsets)),
              b"cmap": cmap, b"name": bytes(name), b"post": post}
    if bold:
        os2 = bytearray(78)
        struct.pack_into(">H", os2, 4, 700)
        struct.pack_into(">H", os2, 6, 5)
        struct.pack_into(">H", os2, 62, 1 << 5)
        tables[b"OS/2"] = bytes(os2)
    if vertical:
        vhea = bytearray(hhea)
        struct.pack_into(">hhhH", vhea, 4, 1500, -500, 0, 2200)
        tables[b"vhea"] = bytes(vhea)
        tables[b"vmtx"] = b"".join(struct.pack(">Hh", 1800 + index * 200, 100 + index * 30) for index in range(count))
    if vorg:
        tables[b"VORG"] = struct.pack(">HHhHHh", 1, 0, 1600, 1, 2, 1700)
    return make_sfnt(tables)


def make_sfnt(tables, cff=False):
    num_tables = len(tables)
    power = 1 << (num_tables.bit_length() - 1)
    result = bytearray(struct.pack(">IHHHH", 0x4F54544F if cff else 0x10000, num_tables, power * 16,
                                   power.bit_length() - 1, num_tables * 16 - power * 16))
    offset = 12 + num_tables * 16
    payload = bytearray()
    head_offset = 0
    for tag, data in sorted(tables.items()):
        result.extend(tag + struct.pack(">III", checksum(data), offset, len(data)))
        if tag == b"head":
            head_offset = offset
        padded = data + bytes((-len(data)) % 4)
        payload.extend(padded)
        offset += len(padded)
    result.extend(payload)
    struct.pack_into(">I", result, head_offset + 8, (0xB1B0AFBA - checksum(result)) & 0xFFFFFFFF)
    return bytes(result)


def make_cid_cff():
    """Original geometric CFF with CID != GID and a real FDArray/FDSelect."""
    def index(items):
        if not items:
            return u16(0)
        offsets = [1]
        for item in items:
            offsets.append(offsets[-1] + len(item))
        return u16(len(items)) + bytes([2]) + b"".join(map(u16, offsets)) + b"".join(items)

    def integer(value):
        return bytes([29]) + struct.pack(">i", value)

    def dictionary(offsets):
        result = integer(391) + integer(392) + integer(0) + bytes([12, 30])
        for offset, operator in zip(offsets, [bytes([15]), bytes([17]), bytes([12, 36]), bytes([12, 37])]):
            result += integer(offset) + operator
        return result

    name = index([b"ZpdfCoverageCID"])
    strings = index([b"Adobe", b"Identity"])
    header = bytes([1, 0, 4, 2])
    prefix = header + name + index([dictionary([0] * 4)]) + strings + index([])
    charset = bytes([0]) + u16(7) + u16(400) + u16(655)
    charstrings = []
    for gid in range(4):
        charstring = bytearray()
        for operands, operator in [([40, 0], 21), ([200 + gid * 23, 0, 0, 420 + gid * 29, -200 - gid * 23, 0], 5), ([], 14)]:
            charstring.extend(b"".join(type1_integer(value) for value in operands))
            charstring.append(operator)
        charstrings.append(bytes(charstring))
    charstrings = index(charstrings)
    fdarray = index([integer(0) + integer(0) + bytes([18])])
    fdselect = bytes(5)
    offsets = [len(prefix), len(prefix) + len(charset),
               len(prefix) + len(charset) + len(charstrings),
               len(prefix) + len(charset) + len(charstrings) + len(fdarray)]
    cff = header + name + index([dictionary(offsets)]) + strings + index([]) + charset + charstrings + fdarray + fdselect
    ttf = make_font("ZpdfCoverageCID", [65, 0x4E2D, 0x20000])
    tables = {}
    for pos in range(12, 12 + int.from_bytes(ttf[4:6], "big") * 16, 16):
        tag = ttf[pos:pos + 4]
        _, offset, length = struct.unpack_from(">III", ttf, pos + 4)
        if tag not in [b"glyf", b"loca"]:
            tables[tag] = ttf[offset:offset + length]
    head = bytearray(tables[b"head"])
    head[8:12] = bytes(4)
    head[50:52] = bytes(2)
    tables[b"head"] = bytes(head)
    tables[b"maxp"] = u32(0x5000) + u16(4)
    tables[b"CFF "] = cff
    return make_sfnt(tables, cff=True)


def encrypt_type1(data, seed):
    result = bytearray()
    for value in data:
        cipher = value ^ (seed >> 8)
        result.append(cipher)
        seed = ((cipher + seed) * 52845 + 22719) & 0xFFFF
    return bytes(result)


def type1_integer(value):
    if -107 <= value <= 107:
        return bytes([value + 139])
    if value >= 108:
        value -= 108
        return bytes([247 + value // 256, value % 256])
    value = -value - 108
    return bytes([251 + value // 256, value % 256])


def make_type1():
    """A geometric PFA decoder fixture with 128 encrypted charstrings."""
    names = ([".notdef"] + list("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz")
             + [f"g{index:03}" for index in range(75)])
    private = bytearray(b"\0\0\0\0dup /Private 8 dict dup begin\n"
                        b"/RD {string currentfile exch readstring pop} executeonly def\n"
                        b"/ND {noaccess def} executeonly def\n"
                        b"/lenIV 4 def /Subrs 0 array def\n")
    private.extend(f"2 index /CharStrings {len(names)} dict dup begin\n".encode())
    for index, name in enumerate(names):
        width = 240 + index % 17 * 23
        height = 420 + index % 11 * 29
        charstring = bytearray(b"\0\0\0\0")
        for operands, operator in [([0, 1000], 13), ([40, 0], 21),
                                   ([width - 40, 0], 5), ([0, height], 5),
                                   ([40 - width, 0], 5), ([], 9), ([], 14)]:
            charstring.extend(b"".join(map(type1_integer, operands)))
            charstring.append(operator)
        encrypted = encrypt_type1(charstring, 4330)
        private.extend(f"/{name} {len(encrypted)} RD ".encode() + encrypted + b" ND\n")
    private.extend(b"end readonly put end readonly put\n"
                   b"dup /FontName get exch definefont pop\nmark currentfile closefile\n")
    header = (b"%!PS-AdobeFont-1.0: ZpdfCoverageType1 1.0\n11 dict begin\n"
              b"/FontName /ZpdfCoverageType1 def /FontType 1 def /PaintType 0 def\n"
              b"/FontMatrix [0.001 0 0 0.001 0 0] readonly def\n"
              b"/FontBBox [0 0 1000 900] readonly def /Encoding StandardEncoding def\n"
              b"currentdict end currentfile eexec\n")
    encrypted = encrypt_type1(private, 55665).hex().upper().encode()
    return header + b"\n".join(encrypted[index:index + 64] for index in range(0, len(encrypted), 64)) + b"\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    directory = Path(__file__).resolve().parent
    cff = make_cid_cff()
    cff_path = directory / "coverage-cid.otf"
    if args.check:
        if cff_path.read_bytes() != cff:
            raise SystemExit("CID-keyed CFF fixture is not reproducible")
    else:
        cff_path.write_bytes(cff)
    fonts = {
        "coverage-primary.ttf": ("ZpdfCoveragePrimary", list(range(32, 127))
                                 + list(range(0x4E00, 0x4F00)) + list(range(0x20000, 0x20010)), False),
        "coverage-fallback.ttf": ("ZpdfCoverageFallback", [32, 0x1F600, 0x1D400]
                                  + list(range(0x4F00, 0x5000)) + list(range(0x20010, 0x20020)), False),
        "coverage-bold.ttf": ("ZpdfCoveragePrimary", [32, 65], True),
        "coverage-uvs.ttf": ("ZpdfCoverageVariation", [32, 0x4E2D, 0x4E2E], False),
        "coverage-cmap.ttf": ("ZpdfCoverageCMap", [32, 65, 0xA5, 0x3001, 0x4E2D, 0x650A, 0x82B3, 0x20087], False),
        "coverage-vertical.ttf": ("ZpdfCoverageVertical", [65, 0x4E2D], False),
        "coverage-vertical-vorg.ttf": ("ZpdfCoverageVerticalVorg", [65, 0x4E2D], False),
    }
    for filename, (family, codepoints, bold) in fonts.items():
        data = make_font(family, codepoints, bold, variations=filename == "coverage-uvs.ttf",
                         vertical=filename.startswith("coverage-vertical"), vorg=filename == "coverage-vertical-vorg.ttf")
        path = directory / filename
        if args.check:
            if path.read_bytes() != data:
                raise SystemExit(f"Fixture is not reproducible: {path}")
        else:
            path.write_bytes(data)
        print(f"{filename}: {len(codepoints)} mapped scalars, {len(data)} bytes")
    # A CAD subset and a broken copy that keeps the same glyph ids but zero
    # `loca` lengths for every glyph, as some AutoCAD Identity-H subsets do.
    cad = [32, 0x41, 0x42, 0x43, 0x44, 0x416]
    for filename, blank in [("cad-subset.ttf", {32}), ("cad-subset-broken.ttf", set(cad))]:
        data = make_font("ZpdfCadSubset", cad, blank=blank)
        path = directory / filename
        if args.check:
            if path.read_bytes() != data:
                raise SystemExit(f"Fixture is not reproducible: {path}")
        else:
            path.write_bytes(data)
        print(f"{filename}: {len(cad)} mapped scalars, {len(data)} bytes")
    path = directory / "coverage-type1.pfa"
    data = make_type1()
    if args.check:
        if path.read_bytes() != data:
            raise SystemExit(f"Fixture is not reproducible: {path}")
    else:
        path.write_bytes(data)
    print(f"{path.name}: 128 encrypted charstrings, {len(data)} bytes")


if __name__ == "__main__":
    main()
