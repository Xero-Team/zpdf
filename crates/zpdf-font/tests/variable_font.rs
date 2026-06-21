//! OpenType variable-font (`fvar`/`gvar`) support: the FontDescriptor-derived
//! variation axes must actually move the glyph outline.
//!
//! The fixture `fixtures/var.ttf` (built with fontTools) has a single `wght`
//! axis (min/default 400, max 900). Glyph `A` (GID 1) is a rectangle whose right
//! edge sits at x=400 in the default master and is pushed to x=700 at the `wght`
//! peak, so the maximum x of its outline is a direct readout of the applied
//! weight.

use zpdf_font::{CidWidths, LoadedFont, OutlineCommand, PdfFontType};

const VAR_TTF: &[u8] = include_bytes!("fixtures/var.ttf");

/// Maximum x-coordinate across a glyph's outline points.
fn max_x(font: &LoadedFont, gid: u16) -> f64 {
    let outline = font.glyph_outline(gid).expect("glyph outline");
    let mut m = f64::MIN;
    for c in &outline.commands {
        match *c {
            OutlineCommand::MoveTo(x, _) | OutlineCommand::LineTo(x, _) => m = m.max(x),
            OutlineCommand::QuadTo(x1, _, x2, _) => m = m.max(x1).max(x2),
            OutlineCommand::CurveTo(x1, _, x2, _, x3, _) => m = m.max(x1).max(x2).max(x3),
            OutlineCommand::Close => {}
        }
    }
    m
}

fn font_at_weight(weight: Option<f64>) -> LoadedFont {
    let mut f = LoadedFont::new_with_data(
        PdfFontType::TrueType,
        "ZpdfVar".into(),
        VAR_TTF.to_vec(),
        CidWidths::new(1000.0),
    );
    if let Some(w) = weight {
        f.set_variations(Some(w), None, None, false);
    }
    f
}

#[test]
fn weight_axis_widens_outline() {
    let default = max_x(&font_at_weight(None), 1); // no axes set → default master
    let light = max_x(&font_at_weight(Some(400.0)), 1);
    let bold = max_x(&font_at_weight(Some(900.0)), 1);

    assert!(
        (default - 400.0).abs() < 1.0,
        "default master right edge ~400, got {default}"
    );
    assert!(
        (light - 400.0).abs() < 1.0,
        "wght=400 right edge ~400, got {light}"
    );
    assert!(
        bold > 650.0,
        "wght=900 should widen 'A' right edge toward 700, got {bold}"
    );
}

#[test]
fn intermediate_weight_interpolates() {
    // wght 650 is halfway across [400, 900] → normalized 0.5 → right edge ~550.
    let mid = max_x(&font_at_weight(Some(650.0)), 1);
    assert!(
        (mid - 550.0).abs() < 20.0,
        "wght=650 should interpolate the right edge to ~550, got {mid}"
    );
}

#[test]
fn weight_axis_varies_advance() {
    // With no /Widths entry, `simple_glyph_advance` falls back to the font's hmtx
    // — which must be read at the *varied* instance (the fixture's advance grows
    // 500→800 across the weight axis), matching the varied outline.
    let light = font_at_weight(Some(400.0)).simple_glyph_advance(0x41, 1);
    let bold = font_at_weight(Some(900.0)).simple_glyph_advance(0x41, 1);
    assert!(
        (light - 500.0).abs() < 1.0,
        "wght=400 advance ~500, got {light}"
    );
    assert!(
        bold > 750.0,
        "wght=900 advance should grow toward 800, got {bold}"
    );
}

#[test]
fn unknown_axes_are_ignored_safely() {
    // The fixture has only a `wght` axis; requesting `wdth`/`slnt`/`ital` (which
    // it lacks) must be a harmless no-op while `wght` still applies — never a
    // panic or a corrupted outline.
    let mut f = font_at_weight(None);
    let before = max_x(&f, 1);
    f.set_variations(Some(900.0), Some(200.0), Some(-12.0), true);
    let after = max_x(&f, 1);
    assert!(
        (before - 400.0).abs() < 1.0 && after > 650.0,
        "wght applies (before {before} → after {after}); absent axes ignored"
    );
}
