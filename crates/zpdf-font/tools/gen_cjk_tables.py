#!/usr/bin/env python3
"""Generate the baked CJK 2-byte -> Unicode (BMP) tables used to decode the
predefined Adobe byte-encoded CMaps for non-embedded CJK fonts.

Each table is produced by decoding every two-byte sequence through a Python
standard-library codec (the same technique used for the hand-baked gb2312.rs),
keeping only entries that decode to a single Basic-Multilingual-Plane scalar.
Pure stdlib -> no new Rust dependency.

Run from the repo root (uses uv per project convention):

    uv run python crates/zpdf-font/tools/gen_cjk_tables.py

Regenerate whenever the encoding coverage needs to change. The emitted .rs
files are committed; do not edit them by hand.
"""

import os

# (module, RUST_CONST, fn_name, python codec, human description, doc CMaps)
TABLES = [
    ("gbk", "GBK", "gbk", "gbk", "GBK (GB2312 superset)",
     "GBK-EUC-H/V, GBKp-EUC-H/V, GBK2K-H/V, GB-EUC-H/V"),
    ("big5", "BIG5", "big5", "cp950", "Big5 (Windows code page 950; ETen/MS extensions)",
     "B5pc-H/V, ETen-B5-H/V, ETenms-B5-H/V, HKscs-B5-H/V, B5-H/V"),
    ("sjis", "SJIS", "sjis", "cp932", "Shift-JIS (Windows-31J / code page 932)",
     "90ms-RKSJ-H/V, 90msp-RKSJ-H/V, 90pv-RKSJ-H, 83pv-RKSJ-H, Add-RKSJ, Ext-RKSJ"),
    ("ksc", "KSC", "ksc", "cp949", "EUC-KR / UHC (Windows code page 949)",
     "KSC-EUC-H/V, KSCms-UHC-H/V, KSCms-UHC-HW-H/V, KSCpc-EUC-H"),
    ("eucjp", "EUCJP", "eucjp", "euc_jp", "EUC-JP",
     "EUC-H/V"),
]

HERE = os.path.dirname(os.path.abspath(__file__))
SRC = os.path.normpath(os.path.join(HERE, "..", "src"))
PER_LINE = 8


def build(codec):
    """Decode every 2-byte sequence; keep single-BMP (non-ASCII) results."""
    out = {}
    for lead in range(0x81, 0xFF):
        for trail in range(0x40, 0xFF):
            try:
                s = bytes([lead, trail]).decode(codec)
            except Exception:
                continue
            if len(s) != 1:
                continue
            cp = ord(s)
            if cp < 0x80 or cp > 0xFFFF:
                continue
            out[(lead << 8) | trail] = cp
    return sorted(out.items())


def main():
    for module, const, fn, codec, desc, cmaps in TABLES:
        entries = build(codec)
        lines = []
        for i in range(0, len(entries), PER_LINE):
            chunk = entries[i:i + PER_LINE]
            lines.append("    " + " ".join(f"({c:#06x}, {u:#06x})," for c, u in chunk))
        body = "\n".join(lines)
        text = f'''//! {desc} 2-byte code -> Unicode (BMP) lookup table.
//!
//! Used to decode the predefined Adobe byte-encoded CMaps
//! ({cmaps}) for non-embedded CJK fonts: the 2-byte code is
//! converted to a Unicode scalar so a substituted system CJK face can resolve
//! the glyph via its Unicode cmap. 1-byte codes are handled by the caller
//! ([`crate::cmap`]).
//!
//! GENERATED FILE - do not edit by hand. Regenerate with
//! `tools/gen_cjk_tables.py` (decodes every 2-byte sequence through Python's
//! `{codec}` codec). Sorted by `code` so it is binary-searchable.

/// `(code, unicode)` pairs, sorted ascending by `code` (big-endian
/// `lead << 8 | trail`).
#[rustfmt::skip]
pub static {const}_TO_UNICODE: &[(u16, u16)] = &[
{body}
];

/// Decode a 2-byte {desc} code to a Unicode scalar, or `None` if unmapped.
pub fn {fn}_to_unicode(code: u16) -> Option<char> {{
    {const}_TO_UNICODE
        .binary_search_by_key(&code, |&(c, _)| c)
        .ok()
        .and_then(|i| char::from_u32({const}_TO_UNICODE[i].1 as u32))
}}
'''
        path = os.path.join(SRC, f"{module}.rs")
        with open(path, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)
        print(f"wrote {os.path.relpath(path)}: {len(entries)} entries")


if __name__ == "__main__":
    main()
