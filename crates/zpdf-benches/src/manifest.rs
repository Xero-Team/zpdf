//! Corpus manifest: integrity + completeness for corpus files that are **not**
//! in version control.
//!
//! Why this exists: the real PDF corpus lives under `tests/`, which the root
//! `.gitignore` excludes wholesale (`/tests`). A fresh clone therefore has
//! *none* of it, and the old corpus loader responded to a missing file with a
//! one-line stderr note and a skip — so `cargo bench` on a fresh clone
//! measured nothing and still exited 0. That silent-degradation behaviour is
//! how the previous round's benchmark numbers rotted unnoticed.
//!
//! This module replaces it with an explicit contract: a checked-in manifest
//! records every local-only fixture's `sha256` + byte size, and the loader
//! **fails hard** on a missing or mismatched file unless the caller explicitly
//! opts into a degraded run (`--synthetic-only`).
//!
//! Files that ARE tracked by git need no manifest entry — git is already a
//! content-addressed integrity mechanism for them.
//!
//! Format (tab-separated, `#` comments, one file per line):
//!
//! ```text
//! # sha256            size     pages class  coverage path
//! 9f2c...             5917614  12    text   29250112 tests/testpdf/example.pdf
//! da41...             307200   -     -      -        tests/failed/batch5/libvips/big.pdf
//! ```
//!
//! `pages`, `class` and `coverage` are `-` when unknown (non-PDF fixtures, or not
//! yet classified). Paths are relative to the workspace root and use `/`. Paths
//! must not contain tabs or newlines.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// One manifest line: a local-only fixture and its expected content identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Lowercase hex SHA-256 of the whole file.
    pub sha256: String,
    /// Exact byte length.
    pub size: u64,
    /// Page count, when known (`None` for non-PDF fixtures).
    pub pages: Option<u32>,
    /// Measured load class (`text`/`vector`/`image`/`mixed`/`empty`), when
    /// classified. See `crate::classify`.
    pub class: Option<String>,
    /// Measured device-pixel coverage of page 0 (the page the benches measure),
    /// when classified.
    ///
    /// Recorded so page *selection* can prefer the heaviest page of each class
    /// without re-measuring: ordering by path alone would drop the corpus's most
    /// expensive page, and file size is not a proxy for coverage (a 5.9 MB file
    /// here holds a 29 Mpx page while a 145 MB one holds 4 Mpx).
    pub coverage_px: Option<u64>,
    /// Workspace-root-relative path, `/`-separated.
    pub path: String,
}

/// A manifest: entries in file order, plus where it was read from.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub entries: Vec<Entry>,
    /// Path the manifest was loaded from, for error messages.
    pub source: PathBuf,
}

/// How strictly to verify. The default is strict: a missing or altered fixture
/// is an error, never a silent skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
    /// Check existence + size + sha256 for every entry. The default.
    Strict,
    /// Check existence + size only. `sha256` is still recorded; use this for a
    /// fast local loop where re-hashing hundreds of MB is unwanted.
    SizeOnly,
}

/// The outcome of a successful verification — the loaded corpus, plus anything
/// the caller should surface.
#[derive(Debug, Clone)]
pub struct Verified {
    pub entries: Vec<Entry>,
    /// Bytes hashed (0 in `SizeOnly`). Useful for a "verified N MB" note.
    pub bytes_hashed: u64,
}

impl Manifest {
    /// Parse a manifest from `text`. `source` is only used for error messages.
    ///
    /// Malformed lines are hard errors: a manifest that silently drops entries
    /// would weaken exactly the guarantee it exists to provide.
    pub fn parse(text: &str, source: &Path) -> Result<Self, String> {
        let mut entries = Vec::new();
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim_end_matches(['\r', '\n']);
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() != 6 {
                return Err(format!(
                    "{}:{}: expected 6 tab-separated fields (sha256, size, pages, class, coverage, path), got {}",
                    source.display(),
                    lineno + 1,
                    fields.len()
                ));
            }
            let sha256 = fields[0].trim().to_ascii_lowercase();
            if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!(
                    "{}:{}: sha256 must be 64 hex chars, got {:?}",
                    source.display(),
                    lineno + 1,
                    fields[0]
                ));
            }
            let size: u64 = fields[1].trim().parse().map_err(|e| {
                format!(
                    "{}:{}: bad size {:?}: {e}",
                    source.display(),
                    lineno + 1,
                    fields[1]
                )
            })?;
            let pages = match fields[2].trim() {
                "-" | "" => None,
                n => Some(n.parse::<u32>().map_err(|e| {
                    format!(
                        "{}:{}: bad page count {n:?}: {e}",
                        source.display(),
                        lineno + 1
                    )
                })?),
            };
            let class = match fields[3].trim() {
                "-" | "" => None,
                c => Some(c.to_string()),
            };
            let coverage_px = match fields[4].trim() {
                "-" | "" => None,
                n => Some(n.parse::<u64>().map_err(|e| {
                    format!(
                        "{}:{}: bad coverage {n:?}: {e}",
                        source.display(),
                        lineno + 1
                    )
                })?),
            };
            let path = fields[5].trim().to_string();
            if path.is_empty() {
                return Err(format!("{}:{}: empty path", source.display(), lineno + 1));
            }
            entries.push(Entry {
                sha256,
                size,
                pages,
                class,
                coverage_px,
                path,
            });
        }
        Ok(Self {
            entries,
            source: source.to_path_buf(),
        })
    }

    /// Read and parse the manifest at `path`.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read manifest {}: {e}", path.display()))?;
        Self::parse(&text, path)
    }

    /// Verify every entry against the filesystem under `root`.
    ///
    /// Returns a [`Verified`] with the entries in manifest order, or a single
    /// error describing *every* problem found (not just the first) so one run
    /// tells the operator everything that is missing or stale.
    pub fn verify(&self, root: &Path, mode: VerifyMode) -> Result<Verified, String> {
        let mut problems = Vec::new();
        let mut bytes_hashed = 0u64;
        for entry in &self.entries {
            let abs = root.join(&entry.path);
            let meta = match std::fs::metadata(&abs) {
                Ok(m) => m,
                Err(e) => {
                    problems.push(format!("{}: missing ({e})", entry.path));
                    continue;
                }
            };
            if meta.len() != entry.size {
                problems.push(format!(
                    "{}: size {} != manifest {}",
                    entry.path,
                    meta.len(),
                    entry.size
                ));
                continue;
            }
            if mode == VerifyMode::SizeOnly {
                continue;
            }
            match sha256_file(&abs) {
                Ok(actual) => {
                    bytes_hashed += entry.size;
                    if actual != entry.sha256 {
                        problems.push(format!(
                            "{}: sha256 {} != manifest {}",
                            entry.path, actual, entry.sha256
                        ));
                    }
                }
                Err(e) => problems.push(format!("{}: hash failed ({e})", entry.path)),
            }
        }
        if !problems.is_empty() {
            let mut msg = format!(
                "corpus manifest verification failed ({} of {} entries) — see {}\n",
                problems.len(),
                self.entries.len(),
                self.source.display()
            );
            for p in &problems {
                let _ = writeln!(msg, "  {p}");
            }
            msg.push_str(
                "\nThis corpus is deliberately NOT in version control (the root \
                 .gitignore excludes /tests).\nRestore the files, or pass \
                 --synthetic-only to run the synthetic corpus alone.\nIf the files \
                 changed on purpose, regenerate with:\n  cargo run -p zpdf-benches \
                 --bin corpus-manifest -- --update\n",
            );
            return Err(msg);
        }
        Ok(Verified {
            entries: self.entries.clone(),
            bytes_hashed,
        })
    }
}

/// SHA-256 a file in 1 MiB chunks. Streaming keeps peak memory flat even for
/// the corpus's largest fixture (a 269 MB PDF in `tests/failed`).
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Render entries as manifest text, with a header comment a reviewer can read in
/// a diff and aligned columns for the numeric fields. Entries are sorted by path
/// so a re-generation produces a minimal diff.
///
/// The field delimiter is a **tab** (paths may contain spaces); the `size`,
/// `pages` and `class` fields are additionally space-padded for readability,
/// which is safe because the parser trims each field.
pub fn render(entries: &mut [Entry]) -> String {
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let w_size = entries
        .iter()
        .map(|e| e.size.to_string().len())
        .max()
        .unwrap_or(1)
        .max(4);
    let w_pages = 5;
    let w_class = entries
        .iter()
        .map(|e| e.class.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(1)
        .max(5);
    let w_cov = entries
        .iter()
        .map(|e| e.coverage_px.map(|c| c.to_string().len()).unwrap_or(1))
        .max()
        .unwrap_or(1)
        .max(8);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# zpdf corpus manifest — files NOT in version control."
    );
    let _ = writeln!(
        out,
        "# Format: sha256<TAB>size<TAB>pages<TAB>class<TAB>coverage<TAB>path   ('-' = unknown)"
    );
    let _ = writeln!(
        out,
        "# coverage = measured device-pixel coverage of page 0; page selection prefers the"
    );
    let _ = writeln!(
        out,
        "# heaviest page per class, so this column is what keeps the expensive pages in the matrix."
    );
    let _ = writeln!(
        out,
        "# Regenerate: cargo run -p zpdf-benches --bin corpus-manifest -- --update"
    );
    let _ = writeln!(
        out,
        "# Regenerate: cargo run -p zpdf-benches --bin corpus-manifest -- --classify"
    );
    let _ = writeln!(
        out,
        "#{:<64} {:<w2$} {:<w3$} {:<w4$} {:<w5$} path",
        "sha256",
        "size",
        "pages",
        "class",
        "coverage",
        w2 = w_size,
        w3 = w_pages,
        w4 = w_class,
        w5 = w_cov,
    );
    for e in entries.iter() {
        let pages = e
            .pages
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string());
        let class = e.class.as_deref().unwrap_or("-");
        let cov = e
            .coverage_px
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".to_string());
        let _ = writeln!(
            out,
            "{}\t{:<w2$}\t{:<w3$}\t{:<w4$}\t{:<w5$}\t{}",
            e.sha256,
            e.size,
            pages,
            class,
            cov,
            e.path,
            w2 = w_size,
            w3 = w_pages,
            w4 = w_class,
            w5 = w_cov,
        );
    }
    out
}

/// Build an entry for `abs`, given the workspace `root` (for the relative
/// path) and an optional page count / class.
pub fn entry_for(
    abs: &Path,
    root: &Path,
    pages: Option<u32>,
    class: Option<String>,
) -> Result<Entry, String> {
    let meta = std::fs::metadata(abs).map_err(|e| format!("stat {}: {e}", abs.display()))?;
    let sha256 = sha256_file(abs).map_err(|e| format!("hash {}: {e}", abs.display()))?;
    let rel = abs
        .strip_prefix(root)
        .map_err(|_| format!("{} is not under {}", abs.display(), root.display()))?;
    let path = rel.to_string_lossy().replace('\\', "/");
    Ok(Entry {
        sha256,
        size: meta.len(),
        pages,
        class,
        coverage_px: None,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }

    /// A scratch directory unique to this process + test, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "zpdf-manifest-{}-{}-{tag}",
                std::process::id(),
                nanos
            ));
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Self(dir)
        }

        fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
            let abs = self.0.join(rel);
            if let Some(p) = abs.parent() {
                std::fs::create_dir_all(p).expect("create parent");
            }
            std::fs::write(&abs, bytes).expect("write fixture");
            abs
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The renderer and parser must agree — a mismatch here silently drops
    /// corpus entries, which is the failure mode this module exists to prevent.
    #[test]
    fn render_parse_round_trip() {
        let src = Path::new("MANIFEST");
        let mut entries = vec![
            Entry {
                sha256: hex('a'),
                size: 1234,
                pages: Some(16),
                class: Some("text".into()),
                coverage_px: Some(250_000),
                path: "tests/test1/1.pdf".into(),
            },
            // A path containing a space must survive (the delimiter is a tab).
            Entry {
                sha256: hex('b'),
                size: 7,
                pages: None,
                class: None,
                coverage_px: None,
                path: "tests/with space/odd name.pdf".into(),
            },
        ];
        let text = render(&mut entries);
        let parsed = Manifest::parse(&text, src).expect("parse rendered manifest");
        // `render` sorts by path; sort our expectation to match.
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(parsed.entries, entries);
        assert_eq!(parsed.entries[1].path, "tests/with space/odd name.pdf");
    }

    #[test]
    fn parse_rejects_malformed_lines() {
        let src = Path::new("MANIFEST");
        // Wrong field count.
        assert!(Manifest::parse("only-one-field\n", src).is_err());
        // Non-hex digest.
        assert!(Manifest::parse(&format!("{}\t1\t-\t-\t-\tp.pdf\n", hex('z')), src).is_err());
        // Non-numeric size.
        assert!(Manifest::parse(&format!("{}\tabc\t-\t-\t-\tp.pdf\n", hex('a')), src).is_err());
        // Empty path.
        assert!(Manifest::parse(&format!("{}\t1\t-\t-\t-\t\n", hex('a')), src).is_err());
    }

    #[test]
    fn parse_accepts_comments_and_blank_lines() {
        let src = Path::new("MANIFEST");
        let text = format!("# a comment\n\n{}\t1\t-\t-\t-\tp.pdf\n", hex('a'));
        let m = Manifest::parse(&text, src).unwrap();
        assert_eq!(m.entries.len(), 1);
    }

    /// A missing fixture is an error, not a skip — the whole point of the module.
    #[test]
    fn verify_fails_on_missing_file() {
        let s = Scratch::new("missing");
        let text = format!("{}\t10\t-\t-\t-\tabsent.pdf\n", hex('a'));
        let m = Manifest::parse(&text, Path::new("MANIFEST")).unwrap();
        let err = m.verify(s.path(), VerifyMode::Strict).unwrap_err();
        assert!(err.contains("missing"), "{err}");
        assert!(err.contains("absent.pdf"), "{err}");
        assert!(
            err.contains("--synthetic-only"),
            "error should hint the escape hatch: {err}"
        );
    }

    #[test]
    fn verify_fails_on_size_mismatch() {
        let s = Scratch::new("size");
        s.write("p.pdf", b"12345");
        let text = format!("{}\t999\t-\t-\t-\tp.pdf\n", hex('a'));
        let m = Manifest::parse(&text, Path::new("MANIFEST")).unwrap();
        let err = m.verify(s.path(), VerifyMode::Strict).unwrap_err();
        assert!(err.contains("size 5 != manifest 999"), "{err}");
    }

    #[test]
    fn verify_strict_detects_hash_mismatch() {
        let s = Scratch::new("hash");
        s.write("p.pdf", b"hello");
        let text = format!("{}\t5\t-\t-\t-\tp.pdf\n", hex('0'));
        let m = Manifest::parse(&text, Path::new("MANIFEST")).unwrap();
        let err = m.verify(s.path(), VerifyMode::Strict).unwrap_err();
        assert!(err.contains("sha256"), "{err}");
    }

    /// `SizeOnly` deliberately skips hashing — a fast local loop, not a
    /// correctness check.
    #[test]
    fn verify_size_only_skips_hash() {
        let s = Scratch::new("sizeonly");
        s.write("p.pdf", b"hello");
        let text = format!("{}\t5\t-\t-\t-\tp.pdf\n", hex('0'));
        let m = Manifest::parse(&text, Path::new("MANIFEST")).unwrap();
        let v = m.verify(s.path(), VerifyMode::SizeOnly).unwrap();
        assert_eq!(v.entries.len(), 1);
        assert_eq!(v.bytes_hashed, 0);
    }

    /// The round-trip of a real file: `entry_for` records the true digest and
    /// `verify` accepts it.
    #[test]
    fn entry_for_then_verify_accepts_real_file() {
        let s = Scratch::new("realfile");
        s.write("sub/p.pdf", b"hello world");
        let abs = s.path().join("sub/p.pdf");
        let e = entry_for(&abs, s.path(), Some(3), Some("mixed".into())).unwrap();
        assert_eq!(e.path, "sub/p.pdf");
        assert_eq!(e.size, 11);
        assert_eq!(e.pages, Some(3));

        let mut entries = vec![e];
        let text = render(&mut entries);
        let m = Manifest::parse(&text, Path::new("MANIFEST")).unwrap();
        let v = m.verify(s.path(), VerifyMode::Strict).unwrap();
        assert_eq!(v.bytes_hashed, 11);
    }

    /// Known-answer test for the digest itself, so a `sha2` bump that changed
    /// the output format (or a bad hex encoding) cannot slip through.
    #[test]
    fn sha256_matches_known_answer() {
        let s = Scratch::new("kat");
        s.write("a.txt", b"abc");
        // FIPS 180-4 test vector for "abc".
        assert_eq!(
            sha256_file(&s.path().join("a.txt")).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
