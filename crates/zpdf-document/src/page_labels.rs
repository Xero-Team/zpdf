//! Page labels (ISO 32000-1 §12.4.2): the printed page "numbers" a viewer shows
//! and a user types to navigate — which are *not* the physical 0-based page
//! indices. A document commonly numbers front matter with lowercase roman
//! numerals (`i, ii, iii …`), the body with decimals (`1, 2, 3 …`), and an
//! appendix with a prefix (`A-1, A-2 …`). The catalog's `/PageLabels` entry is a
//! *number tree* mapping the 0-based index of the first page of each labeling
//! range to a label dictionary describing how that range is numbered.
//!
//! A label dictionary (Table 159) carries:
//!
//! * `/S` — the numbering *style* of the numeric portion: `/D` decimal, `/R` /
//!   `/r` upper/lower roman, `/A` / `/a` upper/lower letters (`A…Z, AA…ZZ, AAA…`).
//!   Absent `/S` means the label has *no* numeric portion (only the prefix).
//! * `/P` — a label *prefix* string prepended to the numeric portion.
//! * `/St` — the numeric value of the *first* page in the range (default `1`,
//!   and `≥ 1`); subsequent pages count up from it.
//!
//! This module reads `/PageLabels` once into the sorted set of ranges and answers
//! "what is page *i*'s label?" ([`PageLabels::label`]). It only reads the object
//! graph; nothing here renders, and (like the other navigation readers) it runs
//! only when explicitly called — never during `open` or rendering.

use std::collections::HashSet;

use zpdf_core::{ObjectId, PdfDict, PdfObject};
use zpdf_parser::PdfFile;

use crate::obj_util::{catalog_dict, resolve_array, resolve_dict, resolve_name, text};

/// Maximum depth of a `/PageLabels` number-tree descent (mirrors the name-tree
/// bound used for destinations).
const MAX_NUMBER_TREE_DEPTH: usize = 64;
/// Cap on tree nodes *and* collected entries materialized while flattening the
/// number tree — bounds a crafted (huge or deeply-nested) tree. Far above any
/// real document, which carries at most a handful of labeling ranges.
const MAX_PAGE_LABEL_ENTRIES: usize = 200_000;
/// Cap on a `/P` prefix's length (in `char`s) carried per range — a real prefix
/// is a few characters (`"A-"`, `"Appendix "`); this bounds an adversarial one.
const MAX_PREFIX_CHARS: usize = 1024;
/// Above this numeric value a roman/letters rendering is both meaningless and a
/// memory hazard (a multi-megabyte run of `M`s or `A`s from a crafted `/St`), so
/// the numeric portion falls back to decimal. No real page label approaches it.
const MAX_FANCY_VALUE: u64 = 100_000;

/// The numbering style of a label's numeric portion (ISO 32000-1 Table 159).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageLabelStyle {
    /// `/D` — decimal arabic numerals (`1, 2, 3, …`).
    Decimal,
    /// `/R` — uppercase roman numerals (`I, II, III, …`).
    RomanUpper,
    /// `/r` — lowercase roman numerals (`i, ii, iii, …`).
    RomanLower,
    /// `/A` — uppercase letters (`A, B, …, Z, AA, BB, …`).
    LettersUpper,
    /// `/a` — lowercase letters (`a, b, …, z, aa, bb, …`).
    LettersLower,
    /// No `/S` — the label has only its `/P` prefix and no numeric portion.
    None,
}

impl PageLabelStyle {
    /// Map a `/S` name to a style; an absent or unrecognized name is [`None`].
    fn from_name(name: Option<&str>) -> Self {
        match name {
            Some("D") => Self::Decimal,
            Some("R") => Self::RomanUpper,
            Some("r") => Self::RomanLower,
            Some("A") => Self::LettersUpper,
            Some("a") => Self::LettersLower,
            _ => Self::None,
        }
    }
}

/// One labeling range: every page from `start` (a 0-based index) up to the next
/// range's start carries this style/prefix, numbered from `first`.
#[derive(Debug, Clone)]
struct LabelRange {
    /// 0-based index of the first page this range labels.
    start: usize,
    /// Numbering style of the numeric portion.
    style: PageLabelStyle,
    /// `/P` prefix, prepended to the numeric portion (possibly empty).
    prefix: String,
    /// `/St` — numeric value at `start` (≥ 1; counts up for later pages).
    first: u64,
}

/// The document's page labels, parsed from `/PageLabels`. Built only when the
/// document declares at least one well-formed labeling range.
#[derive(Debug, Clone)]
pub struct PageLabels {
    /// Ranges sorted ascending by `start`, with duplicate starts collapsed
    /// (first occurrence wins).
    ranges: Vec<LabelRange>,
}

impl PageLabels {
    /// The printed label for a 0-based page index, or `None` when the page falls
    /// *before* the first labeling range (so no range covers it — the document
    /// gave it no label). A range with neither a numeric style nor a prefix
    /// yields `Some("")` — an explicit, deliberately-blank label.
    pub fn label(&self, page_index: usize) -> Option<String> {
        // The covering range is the one with the greatest `start ≤ page_index`.
        let idx = match self.ranges.binary_search_by(|r| r.start.cmp(&page_index)) {
            Ok(i) => i,
            // `Err(0)` — page_index precedes every range's start: uncovered.
            Err(0) => return None,
            Err(i) => i - 1,
        };
        let range = &self.ranges[idx];
        let offset = (page_index - range.start) as u64;
        let value = range.first.saturating_add(offset);
        let numeric = format_numeric(range.style, value);
        Some(format!("{}{}", range.prefix, numeric))
    }
}

/// Parse the catalog's `/PageLabels` number tree. Returns `None` when the
/// document declares no page labels, or the tree yields no usable range.
pub fn parse_page_labels(file: &PdfFile) -> Option<PageLabels> {
    let root = catalog_dict(file)?;
    let tree = resolve_dict(file, root.get("PageLabels"))?;

    let mut entries: Vec<(i64, PdfObject)> = Vec::new();
    let mut visited = HashSet::new();
    // Seed the cycle guard with the tree-root reference itself.
    if let Some(PdfObject::Ref(id)) = root.get("PageLabels") {
        visited.insert(*id);
    }
    let mut budget = MAX_PAGE_LABEL_ENTRIES;
    collect_number_tree(file, &tree, 0, &mut visited, &mut budget, &mut entries);

    let mut ranges: Vec<LabelRange> = Vec::new();
    for (key, value) in entries {
        // A range starts at a non-negative page index; a negative or otherwise
        // unusable key is skipped (it can never cover a real page).
        let Ok(start) = usize::try_from(key) else {
            continue;
        };
        let Some(dict) = resolve_dict(file, Some(&value)) else {
            continue;
        };
        ranges.push(LabelRange {
            start,
            style: PageLabelStyle::from_name(resolve_name(file, dict.get("S")).as_deref()),
            prefix: read_prefix(file, &dict),
            first: read_start_value(file, &dict),
        });
    }

    if ranges.is_empty() {
        return None;
    }

    // Sort by start; a stable sort keeps the first tree occurrence of a duplicate
    // start, then dedup collapses the rest so `label`'s binary search is sound.
    ranges.sort_by_key(|r| r.start);
    ranges.dedup_by_key(|r| r.start);

    Some(PageLabels { ranges })
}

/// Read and bound a label dictionary's `/P` prefix (a text string), decoding
/// UTF-16BE / PDFDoc like the other text-string readers. An absent prefix is the
/// empty string; an adversarially long one is truncated on a `char` boundary.
fn read_prefix(file: &PdfFile, dict: &PdfDict) -> String {
    match text(file, dict, "P") {
        Some(p) if p.chars().count() > MAX_PREFIX_CHARS => {
            p.chars().take(MAX_PREFIX_CHARS).collect()
        }
        Some(p) => p,
        None => String::new(),
    }
}

/// Read a label dictionary's `/St` (the numeric value at the range's first page),
/// following one indirect reference. Per spec `/St` is an integer `≥ 1`; a
/// whole-valued real is accepted too (lax producers, mirroring the
/// `embedded_files` integer helper). An absent, fractional, non-numeric, or
/// out-of-range value clamps to the default `1`.
fn read_start_value(file: &PdfFile, dict: &PdfDict) -> u64 {
    let raw = match dict.get("St") {
        Some(PdfObject::Ref(r)) => file.resolve(*r).ok(),
        Some(other) => Some(other.clone()),
        None => None,
    };
    let n = match raw {
        Some(PdfObject::Integer(n)) => n,
        Some(PdfObject::Real(f)) if f.is_finite() && f.fract() == 0.0 => f as i64,
        _ => return 1,
    };
    if n >= 1 {
        n as u64
    } else {
        1
    }
}

/// Render the numeric portion of a label for `value` in `style`. Returns the
/// empty string for [`PageLabelStyle::None`] (prefix-only labels). A value beyond
/// [`MAX_FANCY_VALUE`] falls back to decimal so a crafted `/St` cannot inflate a
/// roman/letters rendering into a huge string.
fn format_numeric(style: PageLabelStyle, value: u64) -> String {
    use PageLabelStyle::*;
    match style {
        None => String::new(),
        Decimal => value.to_string(),
        // A zero numeric value has no roman/letters form (St ≥ 1 makes this
        // defensive); render nothing rather than an empty or bogus glyph.
        _ if value == 0 => String::new(),
        _ if value > MAX_FANCY_VALUE => value.to_string(),
        RomanUpper => to_roman(value, true),
        RomanLower => to_roman(value, false),
        LettersUpper => to_letters(value, true),
        LettersLower => to_letters(value, false),
    }
}

/// Roman numeral for `value` (`≥ 1`, and bounded by [`MAX_FANCY_VALUE`] at the
/// call site, so the leading run of `M`s stays short). Values above 3999 keep
/// repeating `M` for the thousands, matching mainstream viewers.
fn to_roman(value: u64, upper: bool) -> String {
    const TABLE: [(&str, u64); 13] = [
        ("M", 1000),
        ("CM", 900),
        ("D", 500),
        ("CD", 400),
        ("C", 100),
        ("XC", 90),
        ("L", 50),
        ("XL", 40),
        ("X", 10),
        ("IX", 9),
        ("V", 5),
        ("IV", 4),
        ("I", 1),
    ];
    let mut n = value;
    let mut s = String::new();
    for (sym, v) in TABLE {
        while n >= v {
            s.push_str(sym);
            n -= v;
        }
    }
    if upper {
        s
    } else {
        s.to_ascii_lowercase()
    }
}

/// Letter sequence for `value` (`≥ 1`): `1→A, …, 26→Z, 27→AA, 28→BB, …, 53→AAA`
/// (the letter `(value-1) mod 26`, repeated `⌈value/26⌉` times). Bounded by
/// [`MAX_FANCY_VALUE`] at the call site so the repeat count stays small.
fn to_letters(value: u64, upper: bool) -> String {
    let base = if upper { b'A' } else { b'a' };
    let letter = ((value - 1) % 26) as u8;
    let count = ((value - 1) / 26) + 1;
    let ch = (base + letter) as char;
    std::iter::repeat_n(ch, count as usize).collect()
}

/// Flatten every leaf `key → value` entry of a `/PageLabels` number tree into
/// `out`. A number-tree leaf holds `/Nums [ key0 val0 key1 val1 … ]` with integer
/// keys; interior nodes hold `/Kids` (each optionally `/Limits [lo hi]`, which we
/// do not prune by since we want *all* entries). Bounded by depth, a
/// per-reference visited set, and a shared budget counting each node and each
/// collected entry.
fn collect_number_tree(
    file: &PdfFile,
    node: &PdfDict,
    depth: usize,
    visited: &mut HashSet<ObjectId>,
    budget: &mut usize,
    out: &mut Vec<(i64, PdfObject)>,
) {
    if depth > MAX_NUMBER_TREE_DEPTH || *budget == 0 {
        return;
    }
    *budget -= 1;

    // Leaf: /Nums [ key0 val0 key1 val1 … ], keys ascending. The budget is spent
    // per key/value pair *examined* (not only per integer key collected), so a
    // crafted all-non-integer /Nums array can't be scanned in full for free.
    if let Some(nums) = resolve_array(file, node.get("Nums")) {
        let mut i = 0;
        while i + 1 < nums.len() {
            if *budget == 0 {
                return;
            }
            *budget -= 1;
            if let PdfObject::Integer(k) = nums[i] {
                out.push((k, nums[i + 1].clone()));
            }
            i += 2;
        }
    }

    // Interior: /Kids [ refs ].
    if let Some(kids) = resolve_array(file, node.get("Kids")) {
        for kid in &kids {
            if *budget == 0 {
                return;
            }
            let kid_dict = match kid {
                PdfObject::Ref(r) => {
                    if !visited.insert(*r) {
                        continue;
                    }
                    resolve_dict(file, Some(kid))
                }
                PdfObject::Dict(_) => resolve_dict(file, Some(kid)),
                _ => None,
            };
            let Some(d) = kid_dict else { continue };
            collect_number_tree(file, &d, depth + 1, visited, budget, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::build_pdf;
    use crate::PdfDocument;

    const PAGES: &str = "<< /Type /Pages /Kids [3 0 R] /Count 1 >>";
    const PAGE: &str = "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>";

    fn labels(catalog: &str) -> Option<PageLabels> {
        let doc = PdfDocument::open(build_pdf(&[catalog, PAGES, PAGE])).expect("open");
        doc.page_labels()
    }

    #[test]
    fn no_page_labels_is_none() {
        assert!(labels("<< /Type /Catalog /Pages 2 0 R >>").is_none());
    }

    #[test]
    fn roman_front_matter_then_decimal_body() {
        // Pages 0-3 lowercase roman from i; pages 4+ decimal from 1.
        let pl = labels(
            "<< /Type /Catalog /Pages 2 0 R /PageLabels \
             << /Nums [0 << /S /r >> 4 << /S /D >>] >> >>",
        )
        .expect("labels");
        assert_eq!(pl.label(0).as_deref(), Some("i"));
        assert_eq!(pl.label(1).as_deref(), Some("ii"));
        assert_eq!(pl.label(3).as_deref(), Some("iv"));
        assert_eq!(pl.label(4).as_deref(), Some("1"));
        assert_eq!(pl.label(5).as_deref(), Some("2"));
    }

    #[test]
    fn start_offset_and_prefix() {
        // Appendix: prefix "A-", decimal, first page numbered 1; another range
        // starts numbering at 5 via /St.
        let pl = labels(
            "<< /Type /Catalog /Pages 2 0 R /PageLabels \
             << /Nums [0 << /S /D /P (A-) >> 3 << /S /D /St 5 >>] >> >>",
        )
        .expect("labels");
        assert_eq!(pl.label(0).as_deref(), Some("A-1"));
        assert_eq!(pl.label(2).as_deref(), Some("A-3"));
        assert_eq!(pl.label(3).as_deref(), Some("5"));
        assert_eq!(pl.label(4).as_deref(), Some("6"));
    }

    #[test]
    fn uppercase_roman_and_letters() {
        let pl = labels(
            "<< /Type /Catalog /Pages 2 0 R /PageLabels \
             << /Nums [0 << /S /R >> 3 << /S /A >>] >> >>",
        )
        .expect("labels");
        assert_eq!(pl.label(0).as_deref(), Some("I"));
        assert_eq!(pl.label(2).as_deref(), Some("III"));
        assert_eq!(pl.label(3).as_deref(), Some("A")); // letters value 1
        assert_eq!(pl.label(4).as_deref(), Some("B"));
    }

    #[test]
    fn letters_wrap_past_z() {
        // A range numbered with letters from /St 26 → Z, 27 → AA, 28 → BB.
        let pl = labels(
            "<< /Type /Catalog /Pages 2 0 R /PageLabels \
             << /Nums [0 << /S /a /St 26 >>] >> >>",
        )
        .expect("labels");
        assert_eq!(pl.label(0).as_deref(), Some("z")); // 26
        assert_eq!(pl.label(1).as_deref(), Some("aa")); // 27
        assert_eq!(pl.label(2).as_deref(), Some("bb")); // 28
    }

    #[test]
    fn prefix_only_when_no_style() {
        // No /S: the label is the prefix alone, with no numeric portion.
        let pl = labels(
            "<< /Type /Catalog /Pages 2 0 R /PageLabels \
             << /Nums [0 << /P (Cover) >>] >> >>",
        )
        .expect("labels");
        assert_eq!(pl.label(0).as_deref(), Some("Cover"));
        assert_eq!(pl.label(1).as_deref(), Some("Cover")); // range extends
    }

    #[test]
    fn pages_before_first_range_are_unlabeled() {
        // First range starts at page index 2: pages 0 and 1 have no label.
        let pl = labels(
            "<< /Type /Catalog /Pages 2 0 R /PageLabels \
             << /Nums [2 << /S /D >>] >> >>",
        )
        .expect("labels");
        assert_eq!(pl.label(0), None);
        assert_eq!(pl.label(1), None);
        assert_eq!(pl.label(2).as_deref(), Some("1"));
    }

    #[test]
    fn number_tree_with_kids_interior_node() {
        // /PageLabels as an interior node with a /Kids leaf, not a flat /Nums.
        let doc = PdfDocument::open(build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R /PageLabels << /Kids [4 0 R] >> >>",
            PAGES,
            PAGE,
            "<< /Limits [0 0] /Nums [0 << /S /D /P (p) >>] >>",
        ]))
        .expect("open");
        let pl = doc.page_labels().expect("labels via kids");
        assert_eq!(pl.label(0).as_deref(), Some("p1"));
    }

    #[test]
    fn cyclic_kids_terminate() {
        // A number-tree node listing itself as a kid must not hang.
        let doc = PdfDocument::open(build_pdf(&[
            "<< /Type /Catalog /Pages 2 0 R /PageLabels 4 0 R >>",
            PAGES,
            PAGE,
            "<< /Kids [4 0 R] /Nums [0 << /S /D >>] >>",
        ]))
        .expect("open");
        let pl = doc.page_labels().expect("labels");
        assert_eq!(pl.label(0).as_deref(), Some("1"));
    }

    #[test]
    fn huge_start_value_falls_back_to_decimal() {
        // A crafted /St beyond MAX_FANCY_VALUE must not build a giant roman/letters
        // string — it renders as decimal instead.
        let pl = labels(
            "<< /Type /Catalog /Pages 2 0 R /PageLabels \
             << /Nums [0 << /S /R /St 2000000000 >>] >> >>",
        )
        .expect("labels");
        let l = pl.label(0).expect("label");
        assert_eq!(l, "2000000000");
        assert!(l.len() < 32, "must not expand into a huge roman string");
    }

    #[test]
    fn negative_key_is_skipped() {
        // A negative number-tree key can't index a page; it's dropped, and the
        // valid range still applies.
        let pl = labels(
            "<< /Type /Catalog /Pages 2 0 R /PageLabels \
             << /Nums [-5 << /S /R >> 0 << /S /D >>] >> >>",
        )
        .expect("labels");
        assert_eq!(pl.label(0).as_deref(), Some("1"));
    }

    #[test]
    fn roman_numeral_spot_values() {
        assert_eq!(to_roman(4, true), "IV");
        assert_eq!(to_roman(9, true), "IX");
        assert_eq!(to_roman(40, false), "xl");
        assert_eq!(to_roman(1990, true), "MCMXC");
        assert_eq!(to_roman(2024, false), "mmxxiv");
    }

    #[test]
    fn letter_sequence_spot_values() {
        assert_eq!(to_letters(1, true), "A");
        assert_eq!(to_letters(26, true), "Z");
        assert_eq!(to_letters(27, true), "AA");
        assert_eq!(to_letters(52, false), "zz");
        assert_eq!(to_letters(53, true), "AAA");
    }
}
