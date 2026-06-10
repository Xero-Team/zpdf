//! Regression tests for Quartz (macOS) Type1C subset fonts.
//!
//! Quartz re-encodes text to MacRoman and names subset glyphs after the
//! MacRoman slot, so the PDF /Encoding name → CFF charset lookup must win, and
//! the resolved GID must reach the rasterizer unremapped. Glyphs with no
//! MacRoman-compatible name stay charset SID 0 (".notdef") and are addressed
//! by their original Type 1 code (CMSY minus at code 0).
//!
//! The committed fixture (tests/fixtures/quartz_cff_subset.pdf) embeds two
//! Computer Modern subsets (CMBX12, CMSY10) extracted from a Quartz-produced
//! document; the full document itself lives in the untracked local corpus
//! (tests/test7/1.pdf) and is exercised opportunistically when present.

use zpdf_core::ObjectId;
use zpdf_document::font_loader::load_single_font;
use zpdf_document::PdfDocument;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/quartz_cff_subset.pdf"
);
const CORPUS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/test7/1.pdf");

fn load_font(path: &str, obj: u32) -> zpdf_font::LoadedFont {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let doc = PdfDocument::open(data).expect("parse PDF");
    load_single_font(doc.file(), ObjectId(obj, 0)).expect("load font")
}

/// CMBX12 with /Encoding /MacRomanEncoding: Latin codes must resolve through
/// glyph names to charset GIDs, and glyph_outline must use that GID directly
/// (no built-in-encoding remap — the bug that garbled all Quartz text).
#[test]
fn macroman_names_resolve_to_charset_gids() {
    let font = load_font(FIXTURE, 4);
    assert_eq!(font.base_font, "AAAAAB+YHTVER_CMBX12");

    for (code, gid) in [(b'U', 26), (b'n', 42), (b'q', 45), (b'd', 33)] {
        assert_eq!(
            font.code_to_gid(code as u16),
            Some(gid),
            "code {:?} should map by MacRoman name",
            code as char
        );
        let outline = font.glyph_outline(gid).expect("outline for resolved GID");
        assert!(!outline.commands.is_empty());
    }
}

/// CMSY10 with /Encoding /MacRomanEncoding: the minus glyph is an unnamed
/// charset orphan (SID 0 at GID 1) addressed by its original CMSY code 0 —
/// recovered via /Widths + orphan pairing.
#[test]
fn quartz_orphan_glyph_recovered_at_original_code() {
    let font = load_font(FIXTURE, 7);
    assert_eq!(font.base_font, "AAAAAE+BQVWJI_CMSY10");
    assert_eq!(font.orphan_gids, vec![1]);

    // minus: unencodable in MacRoman, /Widths[0] = 777
    assert_eq!(font.code_to_gid(0), Some(1));
    assert!(font.glyph_outline(1).is_some());

    // nameable glyphs still resolve by MacRoman name
    assert_eq!(font.code_to_gid(b'\\' as u16), Some(6));
    assert_eq!(font.code_to_gid(b'{' as u16), Some(5));
}

/// Same assertions against the original full document, when the local corpus
/// is present (it is intentionally not committed; CI skips this).
#[test]
fn full_corpus_document_when_present() {
    if !std::path::Path::new(CORPUS).exists() {
        eprintln!("skipping: local corpus {CORPUS} not present");
        return;
    }
    let cmbx = load_font(CORPUS, 9);
    assert_eq!(cmbx.base_font, "AAAAAB+YHTVER_CMBX12");
    assert_eq!(cmbx.code_to_gid(b'U' as u16), Some(26));

    let cmsy = load_font(CORPUS, 12);
    assert_eq!(cmsy.base_font, "AAAAAE+BQVWJI_CMSY10");
    assert_eq!(cmsy.orphan_gids, vec![1]);
    assert_eq!(cmsy.code_to_gid(0), Some(1));
}
