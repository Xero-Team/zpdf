use std::{fs, process::Command};
use zpdf_writer::{
    rewrite::{rewrite_pdf, RewriteOptions},
    DocumentBuilder, EncryptionConfig,
};

#[test]
fn password_failures_exit_before_producing_output() {
    let mut b = DocumentBuilder::new();
    let p = b.add_page(100.0, 100.0);
    b.add_text(
        p,
        "secret payload",
        10.0,
        50.0,
        "Helvetica",
        12.0,
        (0.0, 0.0, 0.0),
    )
    .unwrap();
    let doc = zpdf::PdfDocument::open(b.build().unwrap()).unwrap();
    let encrypted = rewrite_pdf(
        doc.file(),
        &RewriteOptions {
            encrypt: Some(EncryptionConfig::aes256("user", "owner")),
            ..Default::default()
        },
    )
    .unwrap();
    let dir = std::env::temp_dir().join(format!(
        "zpdf-auth-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&dir).unwrap();
    let input = dir.join("input.pdf");
    let output = dir.join("output.svg");
    fs::write(&input, encrypted).unwrap();
    for password in [None, Some("wrong")] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_zpdf"));
        cmd.arg("export-svg").arg(&input).arg("-o").arg(&output);
        if let Some(password) = password {
            cmd.args(["--password", password]);
        }
        let result = cmd.output().unwrap();
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            error.contains(if password.is_none() {
                "password required"
            } else {
                "incorrect password"
            }),
            "{error}"
        );
        assert!(!output.exists());
    }
    let result = Command::new(env!("CARGO_BIN_EXE_zpdf"))
        .arg("text")
        .arg(&input)
        .args(["--password", "user"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("secret payload"));
    fs::remove_file(input).unwrap();
    fs::remove_dir(dir).unwrap();
}
