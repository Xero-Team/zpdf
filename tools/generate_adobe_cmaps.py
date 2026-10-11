#!/usr/bin/env python3
"""Generate compact, exact code/CID/Unicode ranges from pinned Adobe sources.

Offline operation uses sources.zip and manifest.json. Never consults a host
codec or installed font. Source inheritance and character-code byte lengths
remain explicit; a Unicode font cmap cannot replace these mappings.
"""

import argparse
import bisect
import hashlib
import heapq
import json
from pathlib import Path
import re
import struct
import zipfile

ROOT = Path(__file__).resolve().parents[1]
DATA = ROOT / "crates/zpdf-font/data/adobe-cmaps"
BLOCK = re.compile(r"(\d+)\s+begin(codespacerange|cidrange|cidchar|bfrange|bfchar)\b(.*?)\bend\2\b", re.S)
TOKEN = re.compile(r"<[0-9a-fA-F\s]*>|\[|\]|\d+")
MAX_CID = 65535
MAX_ENTRIES = 100000


def hexadecimal(value):
    if not value.startswith("<") or not value.endswith(">"):
        raise ValueError(f"Expected a hex string: {value!r}")
    raw = bytes.fromhex(value[1:-1])
    if not raw:
        raise ValueError("Empty source code")
    return raw


def code(value):
    raw = hexadecimal(value)
    if len(raw) > 4:
        raise ValueError("Source code exceeds four bytes")
    return len(raw), int.from_bytes(raw, "big")


def unicode_text(value):
    return hexadecimal(value).decode("utf-16-be", errors="strict")


def parse(source):
    text = re.sub(r"%[^\r\n]*", "", source.decode("ascii"))
    bases = re.findall(r"/([^\s/]+)\s+usecmap\b", text)
    if len(bases) > 1:
        raise ValueError("Multiple source base maps are unsupported")
    ordering = re.search(r"/Ordering\s+\(([^)]+)\)", text)
    wmode = re.search(r"/WMode\s+([01])\b", text)
    result = {"base": bases[0] if bases else None,
              "ordering": ordering.group(1) if ordering else None,
              "wmode": int(wmode.group(1)) if wmode else None,
              "codespaces": [], "ranges": [], "unicode": {}}
    for match in BLOCK.finditer(text):
        count, kind, body = int(match.group(1)), match.group(2), match.group(3)
        if TOKEN.sub("", body).strip():
            raise ValueError(f"Unexpected source token in {kind}")
        tokens = TOKEN.findall(body)
        position = 0
        for _ in range(count):
            length, low = code(tokens[position])
            position += 1
            high = low
            if kind.endswith("range"):
                high_length, high = code(tokens[position])
                position += 1
                if high_length != length or high < low:
                    raise ValueError("Invalid source range")
            if kind == "codespacerange":
                result["codespaces"].append((length, low, high))
                continue
            if high - low + 1 > MAX_ENTRIES:
                raise ValueError("Source range exceeds the generation budget")
            if kind.startswith("cid"):
                first = int(tokens[position])
                position += 1
                if first + high - low > MAX_CID:
                    raise ValueError("CID range exceeds u16")
                result["ranges"].append((length, low, high, first))
                continue
            if low > MAX_CID or high > MAX_CID:
                raise ValueError("Collection Unicode map must be keyed by u16 CIDs")
            if tokens[position] == "[":
                position += 1
                for cid in range(low, high + 1):
                    result["unicode"][cid] = unicode_text(tokens[position])
                    position += 1
                if tokens[position] != "]":
                    raise ValueError("Unicode array has the wrong length")
                position += 1
            else:
                raw = hexadecimal(tokens[position])
                position += 1
                value = int.from_bytes(raw, "big")
                for cid in range(low, high + 1):
                    result["unicode"][cid] = (value + cid - low).to_bytes(len(raw), "big").decode("utf-16-be", errors="strict")
        if position != len(tokens):
            raise ValueError(f"Unexpected operands after {kind}")
    return result


def normalize_ranges(ranges):
    """Flatten overlays by declaration order without expanding huge code spaces."""
    output = []
    for length in range(1, 5):
        rows = [row for row in ranges if row[0] == length]
        events = {}
        for index, (_, low, high, _) in enumerate(rows):
            events.setdefault(low, []).append((True, index))
            events.setdefault(high + 1, []).append((False, index))
        live, priority = set(), []
        points = sorted(events)
        for offset, start in enumerate(points[:-1]):
            for added, index in events[start]:
                if added:
                    live.add(index)
                    heapq.heappush(priority, -index)
                else:
                    live.discard(index)
            while priority and -priority[0] not in live:
                heapq.heappop(priority)
            if not priority:
                continue
            row = rows[-priority[0]]
            end = points[offset + 1] - 1
            first = row[3] + start - row[1]
            if output and output[-1][0] == length and output[-1][2] + 1 == start and output[-1][3] + start - output[-1][1] == first:
                previous = output[-1]
                output[-1] = (length, previous[1], end, previous[3])
            else:
                output.append((length, start, end, first))
    return output


def load_sources(directory):
    manifest = json.loads((directory / "manifest.json").read_text())
    resources = {}
    with zipfile.ZipFile(directory / "sources.zip") as archive:
        if set(archive.namelist()) != set(manifest["files"]):
            raise ValueError("Source archive inventory does not match its manifest")
        for name, record in manifest["files"].items():
            info = archive.getinfo(name)
            if info.file_size != record["size"] or info.file_size > 1024 * 1024:
                raise ValueError(f"Source size mismatch: {name}")
            raw = archive.read(name)
            if hashlib.sha256(raw).hexdigest() != record["sha256"]:
                raise ValueError(f"Source hash mismatch: {name}")
            if "/CMap/" in name or "/pdf2unicode/" in name:
                short = name.rsplit("/", 1)[-1]
                if short in resources:
                    raise ValueError(f"Ambiguous inherited map name: {short}")
                resources[short] = parse(raw)
    cache = {}

    def resolve(name, stack=()):
        if name in stack or len(stack) > 16:
            raise ValueError(f"Cyclic or excessive map inheritance: {name}")
        if name in cache:
            return cache[name]
        own = resources[name]
        base = resolve(own["base"], stack + (name,)) if own["base"] else None
        result = dict(own)
        result["ranges"] = normalize_ranges((base["ranges"] if base else []) + own["ranges"])
        result["codespaces"] = sorted(set((base["codespaces"] if base else []) + own["codespaces"]))
        result["unicode"] = dict(base["unicode"] if base else {})
        result["unicode"].update(own["unicode"])
        result["ordering"] = own["ordering"] or (base["ordering"] if base else None)
        result["wmode"] = own["wmode"] if own["wmode"] is not None else (base["wmode"] if base else 0)
        cache[name] = result
        return result

    for name in resources:
        resolve(name)
    return cache


def scalar(codepoint):
    if codepoint > 0x10FFFF or 0xD800 <= codepoint <= 0xDFFF:
        return None
    return chr(codepoint)


def compile_data(resources):
    text = bytearray()
    text_offsets = {}

    def encode(value):
        if not value:
            return 0xFFFFFFFF
        if len(value) == 1:
            return ord(value)
        if value not in text_offsets:
            text_offsets[value] = len(text)
            raw = value.encode("utf-8")
            text.extend(struct.pack("<H", len(raw)) + raw)
        return 0x80000000 | text_offsets[value]

    canonical = {name.removeprefix("Adobe-").removesuffix("-UCS2"): resource["unicode"]
                 for name, resource in resources.items() if name.startswith("Adobe-") and name.endswith("-UCS2")}
    maps = {}
    binary = bytearray()
    unique_sections = {}

    def section(data):
        if data not in unique_sections:
            unique_sections[data] = len(binary)
            binary.extend(data)
        return unique_sections[data]

    for name, resource in sorted(resources.items()):
        if not resource["ranges"]:
            continue
        pages = {}
        unicode_source = name.startswith("Uni")
        collection = canonical[resource["ordering"]]
        for length, low, high, first in resource["ranges"]:
            for source_code in range(low, high + 1):
                cid = first + source_code - low
                if unicode_source:
                    if "UTF16" in name:
                        value = source_code.to_bytes(length, "big").decode("utf-16-be", errors="strict")
                    else:
                        value = scalar(source_code)
                else:
                    value = collection.get(cid)
                key = (length, source_code >> 8)
                if key not in pages:
                    pages[key] = {}
                pages[key][source_code & 255] = (cid, encode(value))
        groups = []

        def page_descriptor(entries):
            # Unicode-source pages only need CIDs. Reserve 65535 for a miss.
            if unicode_source and any(cid == MAX_CID for cid, _ in entries.values()):
                raise ValueError("Unicode-source map exceeds the compact CID domain")
            implicit = 0x40000000 if unicode_source else 0
            sparse = len(entries) < 64
            if sparse:
                data = struct.pack("<H", len(entries)) + b"".join(
                    struct.pack("<BH", low, cid) if unicode_source else struct.pack("<BHI", low, cid, value)
                    for low, (cid, value) in sorted(entries.items()))
            else:
                data = b"".join(struct.pack("<H", entries.get(low, (MAX_CID, 0))[0]) if unicode_source
                                else struct.pack("<HI", *entries.get(low, (0, 0xFFFFFFFF)))
                                for low in range(256))
            offset = section(data)
            if offset >= 0x40000000:
                raise ValueError("Mapping data exceeds the compact offset domain")
            return offset | implicit | (0x80000000 if sparse else 0)

        for length in range(1, 5):
            descriptors = {prefix: page_descriptor(page)
                           for (page_len, prefix), page in sorted(pages.items()) if page_len == length}
            direct = length <= 2
            if direct:
                data = b"".join(struct.pack("<I", descriptors.get(prefix, 0xFFFFFFFF))
                                for prefix in range(max(descriptors, default=-1) + 1))
            else:
                data = b"".join(struct.pack("<II", prefix, descriptor) for prefix, descriptor in descriptors.items())
            bucket = 0
            if length == 4 and not unicode_source and len(descriptors) > 64:
                prefixes = list(descriptors)
                bounds = b"".join(struct.pack("<I", bisect.bisect_left(prefixes, high << 16)) for high in range(257))
                bucket = section(bounds) + 1
            groups.append({"offset": section(data), "count": len(data) // (4 if direct else 8), "direct": direct, "bucket": bucket})
        maps[name] = {"collection": resource["ordering"], "wmode": resource["wmode"],
                      "codespaces": resource["codespaces"], "groups": groups}
    collections = {}
    for name, values in sorted(canonical.items()):
        count = max(values) + 1
        data = b"".join(struct.pack("<I", encode(values.get(cid))) for cid in range(count))
        collections[name] = {"offset": section(data), "count": count}
    return bytes(binary), bytes(text), {"schema": 5, "sparse_directory_record_bytes": 8,
                                       "page_flags": {"sparse": 0x80000000, "unicode_source": 0x40000000},
                                       "maps": maps, "collections": collections}


def rust_index(index):
    lines = ["// Generated by tools/generate_adobe_cmaps.py from pinned Adobe resources.",
             "// Source identities and the Adobe license are in data/adobe-cmaps/.",
             "#[rustfmt::skip]", "static MAPS: &[PredefinedMap] = &["]
    collection_names = {"CNS1": "Cns1", "GB1": "Gb1", "Japan1": "Japan1", "Korea1": "Korea1", "KR": "Kr"}
    for name, entry in sorted(index["maps"].items()):
        spaces = ", ".join(f"({length}, {low:#x}, {high:#x})" for length, low, high in entry["codespaces"])
        groups = ", ".join(f"({group['offset']}, {group['count']}, {str(group['direct']).lower()}, {group['bucket']})" for group in entry["groups"])
        family = "Utf16" if name.startswith("Uni") and "UTF16" in name else "Ucs2" if name.startswith("Uni") else "Legacy"
        collection = collection_names[entry["collection"]]
        lines.append(f'    PredefinedMap {{ name: "{name}", collection: CharacterCollection::{collection}, wmode: {entry["wmode"]}, encoding: SourceEncoding::{family}, codespaces: &[{spaces}], groups: [{groups}] }},')
    lines.extend(["];", "#[rustfmt::skip]", "static COLLECTIONS: &[(CharacterCollection, usize, usize)] = &["])
    for name, entry in sorted(index["collections"].items()):
        lines.append(f"    (CharacterCollection::{collection_names[name]}, {entry['offset']}, {entry['count']}),")
    lines.extend(["];", ""])
    return "\n".join(lines).encode()


def source_oracle(resources):
    """Test-only digests from source ranges, independent of packed page layout."""
    def digest(rows):
        value, count = 0xCBF29CE484222325, 0
        for length, codepoint, cid, text in rows:
            raw = (text or "").encode("utf-8")
            for byte in struct.pack(">BIHH", length, codepoint, cid, len(raw)) + raw:
                value = ((value ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
            count += 1
        return count, value

    lines = ["// Test oracle derived directly from pinned source ranges, not packed pages.",
             "#[rustfmt::skip]", "const MAP_ORACLE: &[(&str, usize, u64)] = &["]
    for name, resource in sorted(resources.items()):
        if not resource["ranges"]:
            continue
        canonical = resources[f"Adobe-{resource['ordering']}-UCS2"]["unicode"]
        def rows():
            for length, low, high, first in resource["ranges"]:
                for codepoint in range(low, high + 1):
                    cid = first + codepoint - low
                    if name.startswith("Uni"):
                        text = codepoint.to_bytes(length, "big").decode("utf-16-be") if "UTF16" in name else scalar(codepoint)
                    else:
                        text = canonical.get(cid)
                    yield length, codepoint, cid, text
        count, value = digest(rows())
        lines.append(f'    ("{name}", {count}, {value:#018x}),')
    lines.extend(["];", "#[rustfmt::skip]", "const COLLECTION_ORACLE: &[(&str, usize, u64)] = &["])
    for name, resource in sorted(resources.items()):
        if name.startswith("Adobe-") and name.endswith("-UCS2"):
            count, value = digest((2, cid, cid, text) for cid, text in sorted(resource["unicode"].items()))
            ordering = name.removeprefix("Adobe-").removesuffix("-UCS2")
            lines.append(f'    ("{ordering}", {count}, {value:#018x}),')
    lines.extend(["];", ""])
    return "\n".join(lines).encode()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-dir", type=Path, default=DATA)
    parser.add_argument("--output-dir", type=Path, default=DATA)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    resources = load_sources(args.source_dir)
    binary, text, index = compile_data(resources)
    outputs = {"maps.bin": binary, "text.bin": text,
               "index.json": (json.dumps(index, indent=2, sort_keys=True) + "\n").encode(),
               "index.rs": rust_index(index), "test-oracle.rs": source_oracle(resources)}
    for name, data in outputs.items():
        path = args.output_dir / name
        if args.check:
            if path.read_bytes() != data:
                raise ValueError(f"Generated resource is not reproducible: {path}")
        else:
            args.output_dir.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
    print(f"Verified {len(index['maps'])} maps and {len(index['collections'])} collections; {len(binary)} mapping bytes, {len(text)} text bytes")


if __name__ == "__main__":
    main()
