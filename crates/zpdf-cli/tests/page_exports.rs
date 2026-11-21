use std::{fs, process::Command};

fn fixture() -> Vec<u8> {
    let stream = |data: &str, attrs: &str| {
        format!(
            "<< /Length {} {attrs} >>\nstream\n{data}\nendstream",
            data.len()
        )
    };
    let objects=[
        "<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [6 0 R] /D << /BaseState /ON /OFF [6 0 R] >> >> >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 120 80] /UserUnit 2 /Contents 4 0 R /Annots [5 0 R] /Resources << /Properties << /Layer 6 0 R >> /Font << /F1 8 0 R >> >> >>".into(),
        stream("/OC /Layer BDC 0 0 1 rg 0 0 120 80 re f BT /F1 8 Tf 10 60 Td (HIDDEN) Tj 65 0 Td (RIGHT) Tj -65 -20 Td (BOTTOM) Tj 65 0 Td (CELL) Tj ET EMC",""),
        "<< /Type /Annot /Subtype /Square /Rect [40 20 80 60] /AP << /N 7 0 R >> >>".into(),
        "<< /Type /OCG /Name (Hidden) >>".into(),
        stream("1 0 0 rg 0 0 40 40 re f","/Type /XObject /Subtype /Form /BBox [0 0 40 40] /Resources << >>"),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ];
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Root 1 0 R /Size {} >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

#[test]
fn text_search_and_tables_exclude_disabled_layers() {
    let dir = std::env::temp_dir().join(format!("zpdf-hidden-text-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let input = dir.join("in.pdf");
    fs::write(&input, fixture()).unwrap();
    for command in ["text", "search", "tables"] {
        let mut process = Command::new(env!("CARGO_BIN_EXE_zpdf"));
        process.arg(command).arg(&input);
        if command == "search" {
            process.arg("HIDDEN");
        }
        let result = process.output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let text = String::from_utf8_lossy(&result.stdout);
        assert!(
            !text.contains("HIDDEN") && !text.contains("BOTTOM"),
            "{command}: {text}"
        );
    }
    fs::remove_file(input).unwrap();
    fs::remove_dir(dir).unwrap();
}

#[test]
fn cli_exports_keep_annotations_and_hide_disabled_layers() {
    let dir = std::env::temp_dir().join(format!(
        "zpdf-page-exports-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&dir).unwrap();
    let input = dir.join("in.pdf");
    fs::write(&input, fixture()).unwrap();
    for (command, extension) in [("export-svg", "svg"), ("export-pptx", "pptx")] {
        let output = dir.join(format!("out.{extension}"));
        let result = Command::new(env!("CARGO_BIN_EXE_zpdf"))
            .arg(command)
            .arg(&input)
            .arg("-o")
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        if extension == "svg" {
            let svg = fs::read_to_string(&output).unwrap();
            assert!(svg.contains("#ff0000"), "{svg}");
            assert!(!svg.contains("#0000ff"), "{svg}");
            assert!(
                svg.contains("240") && svg.contains("160"),
                "scaled SVG dimensions: {svg}"
            );
        } else {
            assert!(
                String::from_utf8_lossy(&result.stdout).contains("(1 elements)"),
                "{}",
                String::from_utf8_lossy(&result.stdout)
            );
        }
        fs::remove_file(output).unwrap();
    }
    #[cfg(feature = "cpu")]
    {
        let output = dir.join("out.png");
        let result = Command::new(env!("CARGO_BIN_EXE_zpdf"))
            .arg("render")
            .arg(&input)
            .args(["--dpi", "72", "-o"])
            .arg(&output)
            .output()
            .unwrap();
        assert!(result.status.success());
        let bitmap = image::open(&output).unwrap().to_rgba8();
        assert_eq!(bitmap.dimensions(), (240, 160));
        assert_eq!(bitmap.get_pixel(120, 80).0, [255, 0, 0, 255]);
        assert_eq!(bitmap.get_pixel(10, 10).0, [255, 255, 255, 255]);
        fs::remove_file(output).unwrap();
    }
    fs::remove_file(input).unwrap();
    fs::remove_dir(dir).unwrap();
}

fn assemble_raw(objects: &[Vec<u8>]) -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Root 1 0 R /Size {} >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

fn raw_stream(attrs: &str, bytes: &[u8]) -> Vec<u8> {
    let mut out = format!("<< {attrs} /Length {} >>\nstream\n", bytes.len()).into_bytes();
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\nendstream");
    out
}

#[test]
fn corrupt_embedded_font_exports_succeed_when_recovered_and_fail_when_unrecovered() {
    let dir = std::env::temp_dir().join(format!(
        "zpdf-corrupt-font-exports-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&dir).unwrap();

    // 1. Corrupt embedded TrueType referencing Helvetica: when a system substitute
    //    exists, public CLI exports (`render`, `export-svg`, `text`, `convert`)
    //    must recover and succeed rather than failing ensure_complete().
    let recovered_pdf = assemble_raw(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        raw_stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td (ABC) Tj ET"),
        b"<< /Type /Font /Subtype /TrueType /BaseFont /ABCDEF+Helvetica /Encoding /WinAnsiEncoding \
          /FirstChar 65 /LastChar 67 /Widths [620 780 540] /FontDescriptor 6 0 R >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+Helvetica /Flags 32 /FontFile2 7 0 R >>"
            .to_vec(),
        raw_stream("", b"not a valid truetype font"),
    ]);
    let has_system_helvetica = zpdf::PdfDocument::open(recovered_pdf.clone())
        .ok()
        .and_then(|doc| {
            let page = doc.page(0).ok()?;
            let fonts = doc.load_page_fonts(&page);
            Some(fonts.get_by_name("F1")?.1.has_font_data())
        })
        .unwrap_or(false);
    if has_system_helvetica {
        let input = dir.join("recovered.pdf");
        fs::write(&input, recovered_pdf).unwrap();

        let svg_out = dir.join("recovered.svg");
        let svg_res = Command::new(env!("CARGO_BIN_EXE_zpdf"))
            .args(["export-svg"])
            .arg(&input)
            .args(["-o"])
            .arg(&svg_out)
            .output()
            .unwrap();
        assert!(
            svg_res.status.success(),
            "export-svg failed: {}",
            String::from_utf8_lossy(&svg_res.stderr)
        );
        let svg = fs::read_to_string(&svg_out).unwrap();
        assert!(svg.contains("<svg") && svg.contains("<path"), "{svg}");
        fs::remove_file(&svg_out).unwrap();

        let text_res = Command::new(env!("CARGO_BIN_EXE_zpdf"))
            .args(["text"])
            .arg(&input)
            .output()
            .unwrap();
        assert!(
            text_res.status.success(),
            "text failed: {}",
            String::from_utf8_lossy(&text_res.stderr)
        );
        assert!(String::from_utf8_lossy(&text_res.stdout).contains("ABC"));

        let txt_out = dir.join("recovered.txt");
        let conv_res = Command::new(env!("CARGO_BIN_EXE_zpdf"))
            .args(["convert"])
            .arg(&input)
            .args(["-o"])
            .arg(&txt_out)
            .output()
            .unwrap();
        assert!(
            conv_res.status.success(),
            "convert failed: {}",
            String::from_utf8_lossy(&conv_res.stderr)
        );
        assert!(fs::read_to_string(&txt_out).unwrap().contains("ABC"));
        fs::remove_file(&txt_out).unwrap();

        #[cfg(feature = "cpu")]
        {
            let png_out = dir.join("recovered.png");
            let render_res = Command::new(env!("CARGO_BIN_EXE_zpdf"))
                .args(["render"])
                .arg(&input)
                .args(["--dpi", "72", "-o"])
                .arg(&png_out)
                .output()
                .unwrap();
            assert!(
                render_res.status.success(),
                "render failed: {}",
                String::from_utf8_lossy(&render_res.stderr)
            );
            let bitmap = image::open(&png_out).unwrap().to_rgba8();
            let dark_pixels = bitmap
                .pixels()
                .filter(|p| p.0[0] < 200 || p.0[1] < 200 || p.0[2] < 200)
                .count();
            assert!(
                dark_pixels > 50,
                "expected rendered fallback ink, got {dark_pixels}"
            );
            fs::remove_file(&png_out).unwrap();
        }
        fs::remove_file(&input).unwrap();
    }

    // 2. Corrupt Type0 Identity-H without /ToUnicode or CJK collection cannot
    //    recover visible glyphs -> strict exports must fail without overwriting.
    let unrecovered_pdf = assemble_raw(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        raw_stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <00010002> Tj ET"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+UnrecoverableCID /Encoding /Identity-H /DescendantFonts [6 0 R] >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+UnrecoverableCID \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 7 0 R /DW 1000 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+UnrecoverableCID /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        raw_stream("", b"corrupt unrecoverable cid font"),
    ]);
    let bad_input = dir.join("unrecovered.pdf");
    fs::write(&bad_input, unrecovered_pdf).unwrap();

    for (command, ext) in [("export-svg", "svg"), ("render", "png")] {
        #[cfg(not(feature = "cpu"))]
        if command == "render" {
            continue;
        }
        let out = dir.join(format!("unrecovered.{ext}"));
        fs::write(&out, b"existing").unwrap();
        let res = Command::new(env!("CARGO_BIN_EXE_zpdf"))
            .arg(command)
            .arg(&bad_input)
            .args(["-o"])
            .arg(&out)
            .output()
            .unwrap();
        assert!(
            !res.status.success(),
            "{command} should reject unrecovered output"
        );
        let stderr = String::from_utf8_lossy(&res.stderr);
        assert!(
            stderr.contains("incomplete output")
                && stderr.contains("decode_failures=1")
                && stderr.contains("embedded_font_fallbacks=1")
                && stderr.contains("missing_glyph_outlines=1"),
            "{command}: {stderr}"
        );
        assert_eq!(fs::read(&out).unwrap(), b"existing");
        fs::remove_file(&out).unwrap();
    }
    fs::remove_file(&bad_input).unwrap();

    // 3. Corrupt font whose characters all decode via /ToUnicode (e.g. private-use
    //    icon U+E001 absent from system fonts): `text` and `convert` succeed with
    //    the decoded text while `export-svg` and `render` reject missing ink.
    let pua_tounicode = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
        1 begincodespacerange <0000> <FFFF> endcodespacerange \
        1 beginbfchar <0001> <E001> endbfchar \
        endcmap CMapName currentdict /CMap defineresource pop end end";
    let decodable_missing_ink_pdf = assemble_raw(&[
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 180 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        raw_stream("", b"0 0 0 rg BT /F1 24 Tf 15 40 Td <0001> Tj ET"),
        b"<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+UninstalledIconFace /Encoding /Identity-H /DescendantFonts [6 0 R] /ToUnicode 9 0 R >>".to_vec(),
        b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+UninstalledIconFace \
          /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> \
          /FontDescriptor 7 0 R /DW 1000 >>"
            .to_vec(),
        b"<< /Type /FontDescriptor /FontName /ABCDEF+UninstalledIconFace /Flags 32 /FontFile2 8 0 R >>"
            .to_vec(),
        raw_stream("", b"corrupt icon font"),
        raw_stream("", pua_tounicode),
    ]);
    let pua_input = dir.join("decodable_missing_ink.pdf");
    fs::write(&pua_input, decodable_missing_ink_pdf).unwrap();

    let pua_text = Command::new(env!("CARGO_BIN_EXE_zpdf"))
        .args(["text"])
        .arg(&pua_input)
        .output()
        .unwrap();
    assert!(
        pua_text.status.success(),
        "text should succeed when all codes decode: {}",
        String::from_utf8_lossy(&pua_text.stderr)
    );
    assert!(String::from_utf8_lossy(&pua_text.stdout).contains('\u{E001}'));

    let pua_svg = dir.join("pua.svg");
    let pua_svg_res = Command::new(env!("CARGO_BIN_EXE_zpdf"))
        .args(["export-svg"])
        .arg(&pua_input)
        .args(["-o"])
        .arg(&pua_svg)
        .output()
        .unwrap();
    assert!(
        !pua_svg_res.status.success(),
        "export-svg must reject missing ink"
    );
    let pua_stderr = String::from_utf8_lossy(&pua_svg_res.stderr);
    assert!(
        pua_stderr.contains("incomplete output")
            && pua_stderr.contains("missing_glyph_outlines=1")
            && pua_stderr.contains("embedded_font_fallbacks=1"),
        "{pua_stderr}"
    );
    fs::remove_file(&pua_input).unwrap();
    fs::remove_dir(dir).unwrap();
}
