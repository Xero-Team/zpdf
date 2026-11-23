//! Round-trip tests: PDFs authored by DocumentBuilder must parse and render
//! with zpdf's own reader.

use zpdf_document::{PdfDocument, StructRole};
use zpdf_writer::{
    BlendMode, DocumentBuilder, ImageData, PathPaintOptions, PathSegment, PathStyle, TagSpec,
};

#[test]
fn built_pdf_with_path_round_trips() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(200.0, 200.0);
    builder
        .add_path(
            p,
            vec![
                PathSegment::Rect {
                    x: 10.0,
                    y: 10.0,
                    width: 100.0,
                    height: 50.0,
                },
                PathSegment::MoveTo { x: 20.0, y: 100.0 },
                PathSegment::LineTo { x: 150.0, y: 150.0 },
            ],
            PathStyle {
                stroke: Some((0.0, 0.0, 0.0)),
                fill: Some((1.0, 0.0, 0.0)),
                line_width: 2.0,
            },
        )
        .unwrap();
    let bytes = builder.build().unwrap();
    let doc = PdfDocument::open(bytes).unwrap();
    let page = doc.page(0).unwrap();
    let content = doc.file().resolve_stream_data(page.contents[0]).unwrap();
    let text = String::from_utf8_lossy(&content);
    assert!(text.contains("re"), "rect op missing: {text}");
    assert!(text.contains("B"), "fill+stroke op missing: {text}");
}

#[test]
fn nan_path_is_rejected() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(200.0, 200.0);
    let result = builder.add_path(
        p,
        vec![PathSegment::MoveTo {
            x: f64::NAN,
            y: 0.0,
        }],
        PathStyle::default(),
    );
    assert!(result.is_err());
}

#[test]
fn garbage_font_is_rejected() {
    let mut builder = DocumentBuilder::new();
    assert!(builder.embed_font(vec![0u8; 64]).is_err());
}

#[test]
fn built_pdf_opens_and_has_pages() {
    let mut builder = DocumentBuilder::new();
    let p1 = builder.add_page(612.0, 792.0);
    builder
        .add_text(
            p1,
            "Hello from zpdf!",
            72.0,
            720.0,
            "Helvetica",
            24.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    builder
        .add_text(
            p1,
            "Second line",
            72.0,
            680.0,
            "Times-Roman",
            14.0,
            (0.8, 0.1, 0.1),
        )
        .unwrap();
    builder.add_page(400.0, 600.0);

    let bytes = builder.build().unwrap();
    let doc = PdfDocument::open(bytes).unwrap();
    assert_eq!(doc.page_count(), 2);

    let page = doc.page(0).unwrap();
    let rect = page.effective_box();
    assert_eq!((rect.x1 - rect.x0) as u32, 612);
    assert_eq!((rect.y1 - rect.y0) as u32, 792);
}

#[test]
fn built_pdf_content_stream_decodes() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(300.0, 300.0);
    builder
        .add_text(
            p,
            "content (with) \\ escapes",
            10.0,
            200.0,
            "Courier",
            10.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    let bytes = builder.build().unwrap();

    let doc = PdfDocument::open(bytes).unwrap();
    let page = doc.page(0).unwrap();
    let content = doc.file().resolve_stream_data(page.contents[0]).unwrap();
    let text = String::from_utf8_lossy(&content);
    assert!(
        text.contains("BT"),
        "content must contain text block: {text}"
    );
    assert!(text.contains("Tj"), "content must show text: {text}");
    assert!(
        text.contains("content \\(with\\) \\\\ escapes"),
        "string must be escaped: {text}"
    );
}

#[test]
fn built_pdf_with_rgb_image_round_trips() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(200.0, 200.0);
    // 2x2 red/green/blue/white image.
    let pixels = vec![
        255, 0, 0, 0, 255, 0, //
        0, 0, 255, 255, 255, 255,
    ];
    builder
        .add_image(
            p,
            ImageData::Rgb8 {
                width: 2,
                height: 2,
                pixels,
            },
            50.0,
            50.0,
            100.0,
            100.0,
        )
        .unwrap();
    let bytes = builder.build().unwrap();

    let doc = PdfDocument::open(bytes).unwrap();
    let page = doc.page(0).unwrap();
    let content = doc.file().resolve_stream_data(page.contents[0]).unwrap();
    let text = String::from_utf8_lossy(&content);
    assert!(text.contains("/Im1 Do"), "image draw op missing: {text}");
}

#[test]
fn built_pdf_with_rgba_image_has_smask() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(200.0, 200.0);
    let pixels = vec![
        255, 0, 0, 128, 0, 255, 0, 255, 0, 0, 255, 0, 255, 255, 255, 64,
    ];
    builder
        .add_image(
            p,
            ImageData::Rgba8 {
                width: 2,
                height: 2,
                pixels,
            },
            10.0,
            10.0,
            80.0,
            80.0,
        )
        .unwrap();
    let bytes = builder.build().unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("/SMask"), "RGBA image must carry an SMask");

    // The document must still parse.
    let doc = PdfDocument::open(bytes.clone()).unwrap();
    assert_eq!(doc.page_count(), 1);
}

#[test]
fn image_buffer_size_mismatch_is_rejected() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(200.0, 200.0);
    builder
        .add_image(
            p,
            ImageData::Rgb8 {
                width: 10,
                height: 10,
                pixels: vec![0; 5], // wrong size
            },
            0.0,
            0.0,
            10.0,
            10.0,
        )
        .unwrap();
    assert!(builder.build().is_err());
}

#[test]
fn add_path_default_emits_no_ext_gstate() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(200.0, 200.0);
    builder
        .add_path(
            p,
            vec![PathSegment::Rect {
                x: 10.0,
                y: 10.0,
                width: 80.0,
                height: 40.0,
            }],
            PathStyle {
                stroke: Some((0.0, 0.0, 0.0)),
                fill: Some((0.2, 0.5, 0.8)),
                line_width: 2.0,
            },
        )
        .unwrap();
    // Explicit default options must also stay ExtGState-free.
    builder
        .add_path_with_options(
            p,
            vec![PathSegment::Rect {
                x: 100.0,
                y: 10.0,
                width: 50.0,
                height: 40.0,
            }],
            PathStyle::default(),
            PathPaintOptions::default(),
        )
        .unwrap();
    let bytes = builder.build().unwrap();
    let doc = PdfDocument::open(bytes).unwrap();
    let page = doc.page(0).unwrap();
    assert!(
        page.resources.ext_g_state.is_empty(),
        "default paths must not emit /ExtGState resources"
    );
    let content = doc.file().resolve_stream_data(page.contents[0]).unwrap();
    let text = String::from_utf8_lossy(&content);
    assert!(
        !text.contains(" gs\n"),
        "default paths must not emit gs operator: {text}"
    );
}

#[test]
fn path_opacity_and_blend_mode_emit_deduped_ext_gstates_across_pages() {
    let mut builder = DocumentBuilder::new();
    let p1 = builder.add_page(200.0, 200.0);
    let p2 = builder.add_page(200.0, 200.0);

    let highlighter = PathPaintOptions::new()
        .with_fill_opacity(0.4)
        .with_stroke_opacity(0.8)
        .with_blend_mode(BlendMode::Multiply);
    let translucent_zero_neg = PathPaintOptions::new()
        .with_fill_opacity(-0.0)
        .with_stroke_opacity(0.5);
    let translucent_zero_pos = PathPaintOptions::new()
        .with_fill_opacity(0.0)
        .with_stroke_opacity(0.5);

    // Page 1 uses `highlighter` twice and `-0.0`/`+0.0` variants once each.
    for opts in [
        highlighter,
        highlighter,
        translucent_zero_neg,
        translucent_zero_pos,
    ] {
        builder
            .add_path_with_options(
                p1,
                vec![PathSegment::Rect {
                    x: 10.0,
                    y: 10.0,
                    width: 40.0,
                    height: 20.0,
                }],
                PathStyle {
                    fill: Some((1.0, 1.0, 0.0)),
                    stroke: Some((0.0, 0.0, 0.0)),
                    line_width: 1.0,
                },
                opts,
            )
            .unwrap();
    }

    // Page 2 reuses `highlighter` and introduces a distinct Screen config.
    let screen = PathPaintOptions::new()
        .with_opacity(0.75)
        .with_blend_mode(BlendMode::Screen);
    builder
        .add_path_with_options(
            p2,
            vec![PathSegment::Rect {
                x: 20.0,
                y: 20.0,
                width: 60.0,
                height: 30.0,
            }],
            PathStyle {
                fill: Some((0.0, 1.0, 1.0)),
                stroke: None,
                line_width: 1.0,
            },
            highlighter,
        )
        .unwrap();
    builder
        .add_path_with_options(
            p2,
            vec![PathSegment::Rect {
                x: 80.0,
                y: 20.0,
                width: 60.0,
                height: 30.0,
            }],
            PathStyle {
                fill: Some((1.0, 0.0, 1.0)),
                stroke: None,
                line_width: 1.0,
            },
            screen,
        )
        .unwrap();

    let bytes = builder.build().unwrap();
    let doc = PdfDocument::open(bytes).unwrap();
    let page1 = doc.page(0).unwrap();
    let page2 = doc.page(1).unwrap();

    // Page 1 deduplicates the 4 paths down to 2 distinct ExtGState resources.
    assert_eq!(page1.resources.ext_g_state.len(), 2);
    assert_eq!(page2.resources.ext_g_state.len(), 2);

    let p1_gs1 = page1.resources.ext_g_state.get("GS1").expect("page 1 GS1");
    let p1_gs2 = page1.resources.ext_g_state.get("GS2").expect("page 1 GS2");
    let p2_gs1 = page2.resources.ext_g_state.get("GS1").expect("page 2 GS1");
    let p2_gs2 = page2.resources.ext_g_state.get("GS2").expect("page 2 GS2");

    // Identical configuration across pages shares one indirect object.
    assert_eq!(p1_gs1, p2_gs1);
    assert_ne!(p1_gs1, p1_gs2);
    assert_ne!(p2_gs1, p2_gs2);
    assert_ne!(p1_gs2, p2_gs2);

    // Inspect the indirect /ExtGState dictionaries.
    let gs1_obj = doc.file().resolve(*p1_gs1).unwrap();
    let gs1_dict = gs1_obj.as_dict().unwrap();
    assert_eq!(gs1_dict.get_name("Type").unwrap(), "ExtGState");
    assert!((gs1_dict.get_f64("ca").unwrap() - 0.4).abs() < 1e-6);
    assert!((gs1_dict.get_f64("CA").unwrap() - 0.8).abs() < 1e-6);
    assert_eq!(gs1_dict.get_name("BM").unwrap(), "Multiply");

    let gs2_obj = doc.file().resolve(*p1_gs2).unwrap();
    let gs2_dict = gs2_obj.as_dict().unwrap();
    assert_eq!(gs2_dict.get_f64("ca").unwrap(), 0.0);
    assert!((gs2_dict.get_f64("CA").unwrap() - 0.5).abs() < 1e-6);
    assert_eq!(gs2_dict.get_name("BM").unwrap(), "Normal");

    let p2_gs2_obj = doc.file().resolve(*p2_gs2).unwrap();
    let p2_gs2_dict = p2_gs2_obj.as_dict().unwrap();
    assert!((p2_gs2_dict.get_f64("ca").unwrap() - 0.75).abs() < 1e-6);
    assert!((p2_gs2_dict.get_f64("CA").unwrap() - 0.75).abs() < 1e-6);
    assert_eq!(p2_gs2_dict.get_name("BM").unwrap(), "Screen");

    // Content stream isolates every `gs` inside `q ... Q`.
    let content1 = doc.file().resolve_stream_data(page1.contents[0]).unwrap();
    let text1 = String::from_utf8_lossy(&content1);
    assert_eq!(text1.matches("q\n/GS1 gs\n").count(), 2, "{text1}");
    assert_eq!(text1.matches("q\n/GS2 gs\n").count(), 2, "{text1}");
    assert_eq!(text1.matches("\nQ\n").count(), 4, "{text1}");
}

#[test]
fn invalid_path_opacity_returns_error_without_mutating_page() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(200.0, 200.0);
    builder
        .add_text(
            p,
            "Baseline content",
            20.0,
            100.0,
            "Helvetica",
            12.0,
            (0.0, 0.0, 0.0),
        )
        .unwrap();
    let before = builder.build().unwrap();

    let bad_values = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.001, 1.001];
    for bad in bad_values {
        let err_fill = builder.add_path_with_options(
            p,
            vec![PathSegment::Rect {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            }],
            PathStyle::default(),
            PathPaintOptions::new().with_fill_opacity(bad),
        );
        assert!(
            err_fill.is_err(),
            "expected fill_opacity={bad:?} to be rejected"
        );

        let err_stroke = builder.add_path_with_options(
            p,
            vec![PathSegment::Rect {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            }],
            PathStyle::default(),
            PathPaintOptions::new().with_stroke_opacity(bad),
        );
        assert!(
            err_stroke.is_err(),
            "expected stroke_opacity={bad:?} to be rejected"
        );
    }

    // Rejected calls must not leave partial page content or resources behind.
    let after = builder.build().unwrap();
    assert_eq!(
        before, after,
        "failed add_path_with_options must not mutate builder state"
    );
}

#[test]
fn tagged_path_with_options_preserves_ext_gstate_and_structure() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(200.0, 200.0);
    builder
        .add_tagged_path_with_options(
            p,
            vec![PathSegment::Rect {
                x: 10.0,
                y: 10.0,
                width: 100.0,
                height: 40.0,
            }],
            PathStyle {
                fill: Some((1.0, 0.9, 0.0)),
                stroke: None,
                line_width: 1.0,
            },
            PathPaintOptions::new()
                .with_fill_opacity(0.5)
                .with_blend_mode(BlendMode::Multiply),
            TagSpec {
                role: StructRole::Figure,
                alt: Some("Highlight".into()),
                actual_text: None,
            },
        )
        .unwrap();

    let bytes = builder.build().unwrap();
    let doc = PdfDocument::open(bytes).unwrap();
    let page = doc.page(0).unwrap();
    assert_eq!(page.resources.ext_g_state.len(), 1);
    let content = doc.file().resolve_stream_data(page.contents[0]).unwrap();
    let text = String::from_utf8_lossy(&content);
    assert!(text.contains("q\n/GS1 gs\n/Figure <</MCID 0>> BDC\n"));
    assert!(text.contains("EMC\nQ\n"));
    let tree = doc.struct_tree().expect("struct tree present");
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].alt.as_deref(), Some("Highlight"));
}

#[test]
fn tagged_path_escapes_custom_role_name_and_rejects_empty_or_nul() {
    let mut builder = DocumentBuilder::new();
    let p = builder.add_page(200.0, 200.0);

    // Empty and NUL-containing custom roles are rejected without mutating state.
    for bad_role in ["", "Role\0Injected"] {
        let err = builder.add_tagged_path_with_options(
            p,
            vec![PathSegment::Rect {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            }],
            PathStyle::default(),
            PathPaintOptions::new().with_fill_opacity(0.5),
            TagSpec {
                role: StructRole::Other(bad_role.into()),
                alt: None,
                actual_text: None,
            },
        );
        assert!(err.is_err(), "role {bad_role:?} should be rejected");
    }

    // Custom role containing PDF delimiters, spaces, and operators must be
    // encoded as a single `#XX`-escaped PDF name token.
    let hostile = "Fig <</MCID 99>> BDC\n1 0 0 rg /Evil";
    builder
        .add_tagged_path_with_options(
            p,
            vec![PathSegment::Rect {
                x: 10.0,
                y: 10.0,
                width: 40.0,
                height: 20.0,
            }],
            PathStyle {
                fill: Some((0.0, 0.0, 1.0)),
                stroke: None,
                line_width: 1.0,
            },
            PathPaintOptions::new().with_fill_opacity(0.5),
            TagSpec {
                role: StructRole::Other(hostile.into()),
                alt: None,
                actual_text: None,
            },
        )
        .unwrap();

    let bytes = builder.build().unwrap();
    let doc = PdfDocument::open(bytes).unwrap();
    let page = doc.page(0).unwrap();
    let content = doc.file().resolve_stream_data(page.contents[0]).unwrap();
    let text = String::from_utf8_lossy(&content);
    assert!(
        !text.contains("1 0 0 rg"),
        "raw operator from custom role must not appear in content stream: {text}"
    );
    assert!(
        text.contains(
            "/Fig#20#3C#3C#2FMCID#2099#3E#3E#20BDC#0A1#200#200#20rg#20#2FEvil <</MCID 0>> BDC\n"
        ),
        "expected escaped PDF name token in BDC: {text}"
    );
    let tree = doc.struct_tree().expect("struct tree present");
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].role.as_str(), hostile);
}
