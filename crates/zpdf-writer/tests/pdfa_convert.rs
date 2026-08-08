//! PDF/A conversion end-to-end: convert non-conformant PDFs through
//! `rewrite_pdf` with `RewriteOptions::pdfa` and confirm the output passes
//! `zpdf_document::pdfa::validate` for the target profile.

use zpdf_document::pdfa::{validate, Profile};
use zpdf_parser::PdfFile;
use zpdf_writer::{DocumentBuilder, PdfaConvertConfig, PdfaProfile, RewriteOptions};

const VAR_TTF: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../crates/zpdf-font/tests/fixtures/var.ttf"
);

fn conforms(pdf: &[u8], profile: Profile) -> Result<(), String> {
    let file = PdfFile::parse(pdf.to_vec()).expect("parse");
    let report = validate(&file, profile);
    if report.conforms() {
        Ok(())
    } else {
        Err(report
            .violations
            .iter()
            .map(|v| format!("[{}] {}", v.rule, v.message))
            .collect::<Vec<_>>()
            .join(", "))
    }
}

#[test]
fn converts_builder_pdf_to_pdfa1b() {
    let font_bytes = std::fs::read(VAR_TTF).expect("read var.ttf");
    let mut builder = DocumentBuilder::new();
    let page = builder.add_page(612.0, 792.0);
    let font = builder.embed_font(font_bytes).expect("embed");
    builder
        .add_text_embedded(page, "Hello", 72.0, 700.0, font, 24.0, (0.0, 0.0, 0.0))
        .expect("add text");
    let pdf = builder.build().expect("build");

    // The builder output is not PDF/A-1b (no XMP, no output intent, no /ID,
    // header 1.7 > 1.4).
    assert!(
        conforms(&pdf, Profile::A1b).is_err(),
        "builder PDF should not yet conform to PDF/A-1b"
    );

    let file = PdfFile::parse(pdf.to_vec()).expect("parse");
    let converted = zpdf_writer::rewrite_pdf(
        &file,
        &RewriteOptions {
            pdfa: Some(PdfaConvertConfig {
                profile: PdfaProfile::A1b,
                icc: None,
                fallback_font: None,
            }),
            ..Default::default()
        },
    )
    .expect("convert");

    assert!(
        converted.starts_with(b"%PDF-1.4"),
        "A-1b conversion must write a 1.4 header"
    );
    assert!(
        conforms(&converted, Profile::A1b).is_ok(),
        "converted PDF should conform to PDF/A-1b: {}",
        conforms(&converted, Profile::A1b).unwrap_err()
    );
}

#[test]
fn converts_builder_pdf_to_pdfa2b() {
    let font_bytes = std::fs::read(VAR_TTF).expect("read var.ttf");
    let mut builder = DocumentBuilder::new();
    let page = builder.add_page(612.0, 792.0);
    let font = builder.embed_font(font_bytes).expect("embed");
    builder
        .add_text_embedded(page, "Hello", 72.0, 700.0, font, 24.0, (0.0, 0.0, 0.0))
        .expect("add text");
    let pdf = builder.build().expect("build");
    let file = PdfFile::parse(pdf.to_vec()).expect("parse");
    let converted = zpdf_writer::rewrite_pdf(
        &file,
        &RewriteOptions {
            pdfa: Some(PdfaConvertConfig {
                profile: PdfaProfile::A2b,
                icc: None,
                fallback_font: None,
            }),
            ..Default::default()
        },
    )
    .expect("convert");
    assert!(
        converted.starts_with(b"%PDF-1.7"),
        "A-2b keeps a 1.7 header"
    );
    assert!(
        conforms(&converted, Profile::A2b).is_ok(),
        "converted PDF should conform to PDF/A-2b: {}",
        conforms(&converted, Profile::A2b).unwrap_err()
    );
}

/// A minimal PDF whose only font is non-embedded (a TrueType with a
/// FontDescriptor but no FontFile). Converting with a `fallback_font` embeds
/// it as `/FontFile2`, satisfying PDF/A's embedding requirement.
#[test]
fn fallback_font_embeds_nonembedded_font() {
    let objects: &[&str] = &[
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
         /Resources << /Font << /F1 4 0 R >> >> >>",
        "<< /Type /Font /Subtype /TrueType /BaseFont /TestFont /FontDescriptor 5 0 R >>",
        "<< /Type /FontDescriptor /FontName /TestFont /Flags 32 \
         /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 800 /Descent -200 \
         /CapHeight 700 /StemV 80 >>",
    ];
    let pdf = build_pdf(objects);
    // Pre-conversion: fails on fonts, header, XMP, output intent, /ID.
    assert!(conforms(&pdf, Profile::A1b).is_err());

    let file = PdfFile::parse(pdf.to_vec()).expect("parse");
    let fallback = std::fs::read(VAR_TTF).expect("read var.ttf");
    let converted = zpdf_writer::rewrite_pdf(
        &file,
        &RewriteOptions {
            pdfa: Some(PdfaConvertConfig {
                profile: PdfaProfile::A1b,
                icc: None,
                fallback_font: Some(fallback),
            }),
            ..Default::default()
        },
    )
    .expect("convert");
    assert!(
        conforms(&converted, Profile::A1b).is_ok(),
        "fallback-embedded PDF should conform to PDF/A-1b: {}",
        conforms(&converted, Profile::A1b).unwrap_err()
    );
}

/// Lay out a PDF from per-object body strings (object N+1 = `objects[N]`),
/// computing the xref offsets. Mirrors the `build_pdf` test helper used in
/// zpdf-document.
fn build_pdf(objects: &[&str]) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"%PDF-1.7\n");
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(data.len());
        data.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", i + 1, body).as_bytes());
    }
    let xref = data.len();
    data.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    data.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        data.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    data.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    data
}
