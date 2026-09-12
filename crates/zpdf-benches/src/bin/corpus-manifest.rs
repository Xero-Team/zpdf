//! Generate or verify the corpus manifests.
//!
//! The real-PDF corpus is deliberately not in version control (the root
//! `.gitignore` excludes `/tests`), so its identity — and the identity of the
//! oversized `tests/failed` fixtures — is pinned in checked-in manifests
//! instead of in git blobs.
//!
//! Two manifests, two audiences:
//!
//! * `crates/zpdf-benches/corpus-manifest.tsv` — the real-PDF bench corpus.
//!   Verified by the benches at startup (hard failure; see `src/manifest.rs`).
//! * `tests/failed-manifest.tsv` — the `tests/failed` fixtures too large to
//!   track in git. Verified by the robustness harness.
//!
//! The "which files" policy lives here, in `real_corpus_files` /
//! `failed_large_files`. The manifest is the source of truth for *verification*;
//! this binary is the source of truth for *membership*. Change the policy, then
//! re-run `--update` and commit the regenerated manifest.
//!
//! Usage:
//!   cargo run -p zpdf-benches --bin corpus-manifest            # verify
//!   cargo run -p zpdf-benches --bin corpus-manifest -- --update  # regenerate
//!   cargo run -p zpdf-benches --bin corpus-manifest -- --verify-hash

use std::path::{Path, PathBuf};

use zpdf_benches::classify::classify_pages;
use zpdf_benches::manifest::{self, Entry, Manifest, VerifyMode};

/// Directories holding third-party real-world PDFs. Every PDF inside is
/// local-only and gets a manifest entry.
const REAL_CORPUS_DIRS: &[&str] = &[
    "tests/test1",
    "tests/test2",
    "tests/test3",
    "tests/test4",
    "tests/test5",
    "tests/test6",
    "tests/test7",
    "tests/test8",
    "tests/test9",
    "tests/test10",
    "tests/test11",
    "tests/test12",
    "tests/test13",
    "tests/testpdf",
    "tests/zzztest",
];

/// `tests/failed` holds the adversarial bug corpus. Files smaller than this are
/// tracked in git (497 of 618 files, ~30 MB); anything at or above it stays
/// local-only and is pinned here. The threshold exists because GitHub rejects
/// pushes containing a file larger than 100 MB, and one fixture is 269 MB.
const FAILED_IN_REPO_MAX_BYTES: u64 = 1024 * 1024;

/// The bench corpus manifest, relative to the workspace root.
const BENCH_MANIFEST: &str = "crates/zpdf-benches/corpus-manifest.tsv";

/// The oversized-`tests/failed` manifest, relative to the workspace root.
const FAILED_MANIFEST: &str = "tests/failed-manifest.tsv";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let update = args.iter().any(|a| a == "--update");
    // Hashes are the default (the manifest's whole point is content identity);
    // `--no-hash` is the fast local loop, where re-hashing hundreds of MiB on
    // every run is not worth the seconds.
    let no_hash = args.iter().any(|a| a == "--no-hash");
    // Measure each document's load class and write it into the manifest, instead
    // of guessing from a filename (which is how the previous round's corpus got
    // mislabelled).
    let classify = args.iter().any(|a| a == "--classify");
    let help = args.iter().any(|a| a == "--help" || a == "-h");

    if help {
        println!(
            "corpus-manifest — generate, classify, or verify the zpdf corpus manifests\n\n\
             USAGE:\n  \
             cargo run -p zpdf-benches --bin corpus-manifest -- [--update|--classify|--no-hash]\n\n\
             FLAGS:\n  \
             --update    regenerate both manifests from the filesystem\n  \
             --classify  measure each real corpus document's load class and record it\n  \
             --no-hash   existence + size only (skips the sha256 pass)\n  \
             -h, --help  show this help\n\n\
             Manifests:\n  \
             {BENCH_MANIFEST}   (bench corpus, verified by the benches)\n  \
             {FAILED_MANIFEST}  (oversized tests/failed fixtures)"
        );
        return;
    }

    let root = workspace_root();
    let mode = if no_hash {
        VerifyMode::SizeOnly
    } else {
        VerifyMode::Strict
    };

    let result = if classify {
        classify_bench_corpus(&root)
    } else if update {
        update_all(&root)
    } else {
        verify_all(&root, mode)
    };

    if let Err(e) = result {
        eprintln!("corpus-manifest: {e}");
        std::process::exit(1);
    }
}

/// Measure each real-corpus document's load class and record it in the manifest.
///
/// The class is a **measurement**, not a label. The previous round's corpus was
/// labelled by hand and the labels were wrong — the "image-heavy" file was a
/// 346-glyph text page and the "text-heavy" one was a two-image page with zero
/// glyphs — so every conclusion drawn from those labels inherited the error.
///
/// Sampling: first, middle and last page of each document, majority vote. One
/// page can be atypical (a cover, a full-page plate), and a document is not
/// always homogeneous.
fn classify_bench_corpus(root: &Path) -> Result<(), String> {
    const DPI: f32 = 150.0;
    let abs = root.join(BENCH_MANIFEST);
    let mut m = Manifest::load(&abs)?;
    println!(
        "classifying {} documents at {DPI} DPI\n\
         The recorded class is the class of page 0 — the page the benches measure — not a\n\
         majority vote, because a label describing a different page than the measured one is\n\
         exactly how the previous round's corpus went wrong. Sampled classes are printed for\n\
         context.\n",
        m.entries.len()
    );
    for entry in &mut m.entries {
        let path = root.join(&entry.path);
        let samples = classify_pages(&path, DPI, 3)
            .map_err(|e| format!("classification failed for {}: {e}", entry.path))?;
        let (_, class0, comp0) = samples[0];
        let sampled: Vec<String> = samples
            .iter()
            .map(|(i, c, _)| format!("p{i}={}", c.as_str()))
            .collect();
        let (g, v, i) = comp0.weights();
        println!(
            "{}\n  class={}  coverage={} device px (glyphs={g} vector={v} images={i})  sampled=[{}]\n  {}",
            entry.path,
            class0.as_str(),
            g + v + i,
            sampled.join(" "),
            comp0.summary()
        );
        entry.class = Some(class0.as_str().to_string());
        entry.coverage_px = Some(g + v + i);
    }
    write_manifest(root, BENCH_MANIFEST, m.entries)
}

fn update_all(root: &Path) -> Result<(), String> {
    let bench_entries = build_bench_entries(root)?;
    write_manifest(root, BENCH_MANIFEST, bench_entries)?;

    let failed_entries = build_failed_entries(root)?;
    write_manifest(root, FAILED_MANIFEST, failed_entries)?;

    Ok(())
}

fn verify_all(root: &Path, mode: VerifyMode) -> Result<(), String> {
    for rel in [BENCH_MANIFEST, FAILED_MANIFEST] {
        let abs = root.join(rel);
        if !abs.exists() {
            return Err(format!(
                "manifest {} not found — generate it with --update",
                abs.display()
            ));
        }
        let m = Manifest::load(&abs)?;
        let v = m.verify(root, mode)?;
        let note = if mode == VerifyMode::Strict {
            format!(", {} hashed", human_bytes(v.bytes_hashed))
        } else {
            String::new()
        };
        println!(
            "corpus-manifest: {} OK ({} entries{note})",
            rel,
            v.entries.len()
        );
    }
    Ok(())
}

fn write_manifest(root: &Path, rel: &str, mut entries: Vec<Entry>) -> Result<(), String> {
    let abs = root.join(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let text = manifest::render(&mut entries);
    std::fs::write(&abs, text).map_err(|e| format!("write {}: {e}", abs.display()))?;
    println!("corpus-manifest: wrote {} ({} entries)", rel, entries.len());
    Ok(())
}

/// Every PDF in the real-corpus directories, with its page count. Page counts
/// need the parser, so a fixture the parser cannot open is reported rather than
/// silently given a blank count.
fn build_bench_entries(root: &Path) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for dir in REAL_CORPUS_DIRS {
        let dir_abs = root.join(dir);
        if !dir_abs.is_dir() {
            return Err(format!("corpus directory {} is missing", dir_abs.display()));
        }
        let mut files = pdfs_in(&dir_abs)?;
        files.sort();
        for abs in files {
            let pages = page_count(&abs)?;
            out.push(manifest::entry_for(&abs, root, Some(pages), None)?);
        }
    }
    Ok(out)
}

/// The `tests/failed` fixtures too large to track in git.
fn build_failed_entries(root: &Path) -> Result<Vec<Entry>, String> {
    let dir_abs = root.join("tests/failed");
    if !dir_abs.is_dir() {
        return Err(format!("{} is missing", dir_abs.display()));
    }
    let mut out = Vec::new();
    for abs in pdfs_in(&dir_abs)? {
        let size = std::fs::metadata(&abs)
            .map_err(|e| format!("stat {}: {e}", abs.display()))?
            .len();
        if size >= FAILED_IN_REPO_MAX_BYTES {
            out.push(manifest::entry_for(&abs, root, None, None)?);
        }
    }
    Ok(out)
}

/// Recursively collect `*.pdf` (case-insensitive) under `dir`.
fn pdfs_in(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let rd = std::fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for item in rd {
        let item = item.map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
        let path = item.path();
        let ty = item
            .file_type()
            .map_err(|e| format!("file_type {}: {e}", path.display()))?;
        if ty.is_dir() {
            out.extend(pdfs_in(&path)?);
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
        {
            out.push(path);
        }
    }
    Ok(out)
}

/// Page count via the parser — the same code path the benches use, so a fixture
/// that cannot be opened is caught at manifest-generation time.
fn page_count(abs: &Path) -> Result<u32, String> {
    let data = std::fs::read(abs).map_err(|e| format!("read {}: {e}", abs.display()))?;
    let doc = zpdf::PdfDocument::open(data)
        .map_err(|e| format!("parse {} (cannot count pages): {e}", abs.display()))?;
    Ok(doc.page_count() as u32)
}

fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/zpdf-benches; go up two for the workspace root.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or(manifest)
}
