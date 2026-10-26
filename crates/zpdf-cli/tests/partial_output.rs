use std::{fs, process::Command};
use zpdf::{ObjectId, PdfDict, PdfName, PdfObject};
use zpdf_writer::{DocumentBuilder, IncrementalWriter};

#[test]
fn partial_content_cannot_report_success_or_overwrite_an_export() {
    let mut b = DocumentBuilder::new();
    b.add_page(100.0, 100.0);
    let mut w = IncrementalWriter::new(b.build().unwrap()).unwrap();
    let page = w.document().page(0).unwrap();
    let mut dict = w
        .document()
        .file()
        .resolve(page.id)
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    let mut bad = PdfDict::new();
    bad.insert(
        PdfName::new("Filter"),
        PdfObject::Name(PdfName::new("UnsupportedDecode")),
    );
    let (n, g) = w.try_add_stream(&bad, b"bad").unwrap();
    dict.insert(
        PdfName::new("Contents"),
        PdfObject::Ref(ObjectId(n, g as u16)),
    );
    w.overwrite_object(page.id, PdfObject::Dict(dict));
    let mut bytes = std::io::Cursor::new(Vec::new());
    w.write(&mut bytes).unwrap();
    let dir = std::env::temp_dir().join(format!(
        "zpdf-partial-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&dir).unwrap();
    let input = dir.join("input.pdf");
    fs::write(&input, bytes.into_inner()).unwrap();
    for (command, extension) in [
        ("convert", "txt"),
        ("export-svg", "svg"),
        ("export-pptx", "pptx"),
    ] {
        let output = dir.join(format!("output.{extension}"));
        fs::write(&output, b"existing output").unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_zpdf"))
            .arg(command)
            .arg(&input)
            .arg("-o")
            .arg(&output)
            .output()
            .unwrap();
        assert!(!result.status.success(), "{command}");
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            error.contains("incomplete output") && error.contains("decode_failures=1"),
            "{error}"
        );
        assert_eq!(fs::read(&output).unwrap(), b"existing output");
        fs::remove_file(output).unwrap();
    }
    fs::remove_file(input).unwrap();
    fs::remove_dir(dir).unwrap();
}
