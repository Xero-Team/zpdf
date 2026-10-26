use std::{fs, process::Command};

#[test]
fn invalid_redaction_arguments_never_create_output() {
    let dir = std::env::temp_dir().join(format!(
        "zpdf-redact-args-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&dir).unwrap();
    let input = dir.join("in.pdf");
    let output = dir.join("out.pdf");
    let mut builder = zpdf_writer::DocumentBuilder::new();
    builder.add_page(100.0, 100.0);
    fs::write(&input, builder.build().unwrap()).unwrap();
    for args in [
        vec!["-p", "abc"],
        vec!["-p", "0"],
        vec!["-p", "-1"],
        vec!["-p", "999999999999999999999999999"],
        vec!["-p"],
        vec!["--unknown"],
        vec!["--rect", "0,0,oops,10,10"],
        vec!["--rect", "0,0,NaN,10"],
        vec!["-p", "2"],
        vec!["unexpected.pdf"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_zpdf"))
            .arg("redact")
            .arg(&input)
            .args(["--rect", "0,0,10,10", "--no-fill", "-o"])
            .arg(&output)
            .args(&args)
            .output()
            .unwrap();
        assert!(!result.status.success(), "accepted {args:?}");
        assert!(!output.exists(), "created output for {args:?}");
    }
    let result = Command::new(env!("CARGO_BIN_EXE_zpdf"))
        .arg("redact")
        .arg(&input)
        .args(["-p", "1", "--rect", "0,0,10,10", "--no-fill", "-o"])
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    fs::remove_file(output).unwrap();
    fs::remove_file(input).unwrap();
    fs::remove_dir(dir).unwrap();
}
