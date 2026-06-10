//! Regression tests for Quartz (macOS) Type1C subset fonts (tests/test7/1.pdf).
//!
//! Quartz re-encodes text to MacRoman and names subset glyphs after the
//! MacRoman slot, so the PDF /Encoding name → CFF charset lookup must win, and
//! the resolved GID must reach the rasterizer unremapped. Glyphs with no
//! MacRoman-compatible name stay charset SID 0 (".notdef") and are addressed
//! by their original Type 1 code (CMSY minus at code 0).

use zpdf_core::ObjectId;
use zpdf_document::font_loader::load_single_font;
use zpdf_document::PdfDocument;

const PDF_PATH: &str = "../../tests/test7/1.pdf";

fn load_font(obj: u32) -> zpdf_font::LoadedFont {
    let data = std::fs::read(PDF_PATH).expect("tests/test7/1.pdf present");
    let doc = PdfDocument::open(data).expect("parse test7");
    load_single_font(doc.file(), ObjectId(obj, 0)).expect("load font")
}

/// Object 9: AAAAAB+YHTVER_CMBX12, /Encoding /MacRomanEncoding.
/// Latin codes must resolve through glyph names to charset GIDs, and
/// glyph_outline must use that GID directly (no built-in-encoding remap).
#[test]
fn macroman_names_resolve_to_charset_gids() {
    let font = load_font(9);
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

/// Object 12: AAAAAE+BQVWJI_CMSY10, /Encoding /MacRomanEncoding. The minus
/// glyph is an unnamed charset orphan (SID 0 at GID 1) addressed by its
/// original CMSY code 0 — recovered via /Widths + orphan pairing.
#[test]
fn quartz_orphan_glyph_recovered_at_original_code() {
    let font = load_font(12);
    assert_eq!(font.base_font, "AAAAAE+BQVWJI_CMSY10");
    assert_eq!(font.orphan_gids, vec![1]);

    // minus: unencodable in MacRoman, /Widths[0] = 777
    assert_eq!(font.code_to_gid(0), Some(1));
    assert!(font.glyph_outline(1).is_some());

    // nameable glyphs still resolve by MacRoman name
    assert_eq!(font.code_to_gid(b'\\' as u16), Some(6));
    assert_eq!(font.code_to_gid(b'{' as u16), Some(5));
}
