//! PDF/A conversion end-to-end: convert non-conformant PDFs through
//! `rewrite_pdf` with `RewriteOptions::pdfa` and confirm the output passes
//! `zpdf_document::pdfa::validate` for the target profile.

use zpdf_core::PdfObject;
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

// ---- Type0/CID fallback embedding ----------------------------------------

/// A non-embedded Type0 font (a Type0 with a descendant CIDFont whose
/// FontDescriptor has no FontFile). Converting with a `fallback_font` embeds
/// it on the descendant as `/FontFile2` with `/CIDToGIDMap /Identity`, so the
/// font-embedding rule of PDF/A passes.
#[test]
fn fallback_font_embeds_nonembedded_type0_font() {
    let objects: &[&str] = &[
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
         /Resources << /Font << /F1 4 0 R >> >> >>",
        // Type0 font → descendant CIDFont → FontDescriptor (no FontFile).
        "<< /Type /Font /Subtype /Type0 /BaseFont /TestType0 \
         /Encoding /Identity-H /DescendantFonts [5 0 R] >>",
        "<< /Type /Font /Subtype /CIDFontType2 /BaseFont /TestType0 \
         /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
         /FontDescriptor 6 0 R /DW 1000 >>",
        "<< /Type /FontDescriptor /FontName /TestType0 /Flags 4 \
         /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 800 /Descent -200 \
         /CapHeight 700 /StemV 80 >>",
    ];
    let pdf = build_pdf(objects);
    // Pre-conversion: fails (non-embedded font, no XMP/intent/ID, 1.7 header).
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

    // The converted output embeds a FontFile2 and uses /CIDToGIDMap /Identity
    // on the descendant, and conforms to PDF/A-1b (font embedding satisfied).
    let body = String::from_utf8_lossy(&converted);
    assert!(
        body.contains("/FontFile2"),
        "Type0 fallback must embed /FontFile2"
    );
    assert!(
        body.contains("/CIDToGIDMap /Identity"),
        "Type0 fallback must set /CIDToGIDMap /Identity: {body}"
    );
    assert!(
        conforms(&converted, Profile::A1b).is_ok(),
        "Type0 fallback-embedded PDF should conform to PDF/A-1b: {}",
        conforms(&converted, Profile::A1b).unwrap_err()
    );
}

// ---- Forbidden-annotation removal ----------------------------------------

/// A page carrying a PDF/A-forbidden annotation subtype (Sound) is cleaned
/// during conversion: the annotation is dropped from the page's `/Annots`,
/// and the converted output conforms. Also exercises a JavaScript annotation
/// action (forbidden too). The annotation object may linger in the file
/// (unreferenced), so we check the page's reachable `/Annots`, not the bytes.
#[test]
fn conversion_strips_forbidden_annotations() {
    let objects: &[&str] = &[
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
         /Resources << /Font << /F1 4 0 R >> >> \
         /Annots [6 0 R 7 0 R 8 0 R] >>",
        // A non-embedded TrueType with a FontDescriptor, so the fallback_font
        // path can embed it and the font rule passes (isolating the annot test).
        "<< /Type /Font /Subtype /TrueType /BaseFont /TestFont /FontDescriptor 5 0 R >>",
        "<< /Type /FontDescriptor /FontName /TestFont /Flags 32 \
         /FontBBox [0 0 1000 1000] /ItalicAngle 0 /Ascent 800 /Descent -200 \
         /CapHeight 700 /StemV 80 >>",
        // Forbidden: /Subtype /Sound.
        "<< /Type /Annot /Subtype /Sound /Rect [0 0 50 50] /Contents (beep) >>",
        // Permitted: /Subtype /Text (a note) — must be kept.
        "<< /Type /Annot /Subtype /Text /Rect [60 60 80 80] /Contents (note) >>",
        // Forbidden via its action: a Launch action annotation.
        "<< /Type /Annot /Subtype /Link /Rect [100 100 120 120] \
         /A << /S /Launch /F (calc.exe) >> >>",
    ];
    let pdf = build_pdf(objects);
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

    // Parse the converted PDF and inspect the page's reachable /Annots.
    let cfile = PdfFile::parse(converted.to_vec()).expect("parse converted");
    let subtypes = page_annot_subtypes(&cfile);
    assert!(
        !subtypes.iter().any(|s| s == "Sound"),
        "Sound annotation must be removed from /Annots, got: {subtypes:?}"
    );
    assert!(
        !subtypes.iter().any(|s| s == "Link"),
        "Launch-action Link must be removed from /Annots, got: {subtypes:?}"
    );
    assert!(
        subtypes.iter().any(|s| s == "Text"),
        "permitted Text annotation must be kept, got: {subtypes:?}"
    );
    assert!(
        conforms(&converted, Profile::A1b).is_ok(),
        "annotation-cleaned PDF should conform to PDF/A-1b: {}",
        conforms(&converted, Profile::A1b).unwrap_err()
    );
}

/// A FileAttachment annotation is forbidden under PDF/A-1b (carries an
/// embedded file) but permitted under PDF/A-2b.
#[test]
fn fileattachment_stripped_under_a1b_kept_under_a2b() {
    let objects: &[&str] = &[
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
         /Resources << /Font << /F1 4 0 R >> >> /Annots [5 0 R] >>",
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        "<< /Type /Annot /Subtype /FileAttachment /Rect [0 0 50 50] \
         /Contents (attach) /FS << /Type /EmbeddedFile /F (x.txt) >> >>",
    ];
    let pdf = build_pdf(objects);
    let file = PdfFile::parse(pdf.to_vec()).expect("parse");

    // A-1b strips the FileAttachment from the page's /Annots.
    let a1b = zpdf_writer::rewrite_pdf(
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
    .expect("convert a1b");
    let a1b_file = PdfFile::parse(a1b.to_vec()).expect("parse a1b");
    let a1b_subs = page_annot_subtypes(&a1b_file);
    assert!(
        !a1b_subs.iter().any(|s| s == "FileAttachment"),
        "A-1b must strip FileAttachment, got: {a1b_subs:?}"
    );

    // A-2b keeps it (embedded files are permitted in A-2).
    let a2b = zpdf_writer::rewrite_pdf(
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
    .expect("convert a2b");
    let a2b_file = PdfFile::parse(a2b.to_vec()).expect("parse a2b");
    let a2b_subs = page_annot_subtypes(&a2b_file);
    assert!(
        a2b_subs.iter().any(|s| s == "FileAttachment"),
        "A-2b should keep FileAttachment, got: {a2b_subs:?}"
    );
}

// ---- PDF/A-3b conversion ------------------------------------------------

/// A builder PDF (embedded font, no attachments) converts to PDF/A-3b and the
/// output passes `validate --profile pdfa-3b` (the A-3b AF rules are silent
/// because there are no embedded files).
#[test]
fn converts_builder_pdf_to_pdfa3b() {
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
                profile: PdfaProfile::A3b,
                icc: None,
                fallback_font: None,
            }),
            ..Default::default()
        },
    )
    .expect("convert");
    assert!(
        converted.starts_with(b"%PDF-1.7"),
        "A-3b keeps a 1.7 header"
    );
    assert!(
        conforms(&converted, Profile::A3b).is_ok(),
        "converted PDF should conform to PDF/A-3b: {}",
        conforms(&converted, Profile::A3b).unwrap_err()
    );
    // The XMP packet must declare part 3 / conformance B. The XMP stream is
    // FlateDecode-compressed in the output, so check via the validator's
    // `claimed` field rather than a raw-byte substring.
    let cfile = PdfFile::parse(converted.to_vec()).expect("parse converted");
    let report = zpdf_document::pdfa::validate(&cfile, Profile::A3b);
    assert_eq!(
        report
            .claimed
            .as_ref()
            .map(|(p, c)| (p.as_str(), c.as_str())),
        Some(("3", "B")),
        "A-3b XMP must claim pdfaid:part=3 conformance=B, got: {:?}",
        report.claimed
    );
}

/// A document with `/Names /EmbeddedFiles` but no `/AF` converts to PDF/A-3b
/// by synthesizing the catalog `/AF` array (and `/AFRelationship /Unspecified`
/// on the filespec). The output then passes `validate --profile pdfa-3b`.
#[test]
fn a3b_synthesizes_af_for_embedded_files() {
    let objects: &[&str] = &[
        "<< /Type /Catalog /Pages 2 0 R /Names << /EmbeddedFiles 4 0 R >> >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
        // 4: name-tree root with one leaf entry → filespec 5.
        "<< /Names [ (invoice.xml) 5 0 R ] >>",
        // 5: filespec with an embedded stream (6) but NO /AFRelationship.
        "<< /Type /Filespec /UF (invoice.xml) /EF << /F 6 0 R >> >>",
        // 6: embedded-file stream.
        "<< /Type /EmbeddedFile /Subtype /application#2Fxml /Length 7 >>\n\
         stream\n<x></x>\nendstream",
    ];
    let pdf = build_pdf(objects);
    let file = PdfFile::parse(pdf.to_vec()).expect("parse");
    let converted = zpdf_writer::rewrite_pdf(
        &file,
        &RewriteOptions {
            pdfa: Some(PdfaConvertConfig {
                profile: PdfaProfile::A3b,
                icc: None,
                fallback_font: None,
            }),
            ..Default::default()
        },
    )
    .expect("convert");

    // The converted output must conform to PDF/A-3b (AF synthesized).
    assert!(
        conforms(&converted, Profile::A3b).is_ok(),
        "A-3b conversion with synthesized /AF should conform: {}",
        conforms(&converted, Profile::A3b).unwrap_err()
    );

    // The catalog must now carry /AF, and the filespec must carry
    // /AFRelationship /Unspecified.
    let cfile = PdfFile::parse(converted.to_vec()).expect("parse converted");
    let body = String::from_utf8_lossy(&converted);
    assert!(
        body.contains("/AF"),
        "A-3b conversion must synthesize a catalog /AF: {body}"
    );
    assert!(
        body.contains("/AFRelationship /Unspecified"),
        "A-3b conversion must add /AFRelationship /Unspecified: {body}"
    );
    // The synthesized /AF must point at a reachable filespec (resolve the
    // catalog and check /AF is a non-empty array of refs).
    let root = cfile.trailer.get_ref("Root").expect("Root");
    let catalog = cfile
        .resolve(root)
        .and_then(|o| o.as_dict().cloned())
        .expect("catalog");
    let af = catalog.get("AF").and_then(|o| match o {
        PdfObject::Array(a) => Some(a.clone()),
        _ => None,
    });
    let af = af.expect("catalog /AF array present");
    assert!(
        af.iter().any(|o| matches!(o, PdfObject::Ref(_))),
        "synthesized /AF must contain filespec references: {af:?}"
    );
}

/// A-3b permits FileAttachment annotations (it permits embedded files),
/// matching A-2b — the converter must not strip them.
#[test]
fn a3b_keeps_fileattachment() {
    let objects: &[&str] = &[
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Annots [4 0 R] >>",
        "<< /Type /Annot /Subtype /FileAttachment /Rect [0 0 50 50] \
         /Contents (attach) /FS << /Type /EmbeddedFile /F (x.txt) >> >>",
    ];
    let pdf = build_pdf(objects);
    let file = PdfFile::parse(pdf.to_vec()).expect("parse");
    let converted = zpdf_writer::rewrite_pdf(
        &file,
        &RewriteOptions {
            pdfa: Some(PdfaConvertConfig {
                profile: PdfaProfile::A3b,
                icc: None,
                fallback_font: None,
            }),
            ..Default::default()
        },
    )
    .expect("convert");
    let cfile = PdfFile::parse(converted.to_vec()).expect("parse converted");
    let subs = page_annot_subtypes(&cfile);
    assert!(
        subs.iter().any(|s| s == "FileAttachment"),
        "A-3b should keep FileAttachment, got: {subs:?}"
    );
}

/// Resolve the first page's `/Annots` and return the `/Subtype` of each
/// reachable annotation. Used to verify forbidden annotations were dropped
/// from the page (rather than lingering unreferenced in the file body).
fn page_annot_subtypes(file: &PdfFile) -> Vec<String> {
    let root = file.trailer.get_ref("Root").expect("Root");
    let catalog = file
        .resolve(root)
        .and_then(|o| o.as_dict().cloned())
        .expect("catalog");
    let pages = catalog.get_ref("Pages").expect("Pages");
    let pages_dict = file
        .resolve(pages)
        .and_then(|o| o.as_dict().cloned())
        .expect("pages");
    let page_ref = match pages_dict.get("Kids").and_then(|o| match o {
        PdfObject::Array(a) => a.first().cloned(),
        _ => None,
    }) {
        Some(PdfObject::Ref(r)) => r,
        _ => panic!("no page kid"),
    };
    let page = file
        .resolve(page_ref)
        .and_then(|o| o.as_dict().cloned())
        .expect("page");
    let Some(PdfObject::Array(annots)) = page.get("Annots") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for a in annots {
        let resolved = match a {
            PdfObject::Ref(r) => file.resolve(*r).unwrap_or(zpdf_core::PdfObject::Null),
            other => other.clone(),
        };
        if let Ok(d) = resolved.as_dict() {
            if let Ok(s) = d.get_name("Subtype") {
                out.push(s.to_string());
            }
        }
    }
    out
}
