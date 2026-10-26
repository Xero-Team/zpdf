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
