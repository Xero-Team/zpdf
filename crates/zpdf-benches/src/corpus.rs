//! Corpus selection and integrity.
//!
//! Two corpora back the benches:
//!
//! * **synthetic** — `tests/corpus/*.pdf`, 9 tiny single-feature PDFs built by
//!   `tests/gen_corpus.py`. In version control, so always present.
//! * **real** — third-party PDFs under `tests/<dir>/`, which the root
//!   `.gitignore` excludes outright. Their identity is pinned in
//!   `crates/zpdf-benches/corpus-manifest.tsv` (`sha256` + size + page count),
//!   and [`Corpus::load`] **fails hard** when a fixture is missing or altered.
//!
//! The hard failure is the point. The previous loader skipped missing files
//! with a stderr note, so on any machine without the corpus `cargo bench`
//! measured nothing and exited 0 — which is how the last round's numbers went
//! stale without anyone noticing. `ZPDF_BENCH_SYNTHETIC_ONLY=1` is the one
//! explicit, loudly-announced way to run degraded.

use std::path::{Path, PathBuf};

use crate::classify::LoadClass;
use crate::manifest::{Entry, Manifest, Verified, VerifyMode};

/// In-repo synthetic corpus directory, relative to the workspace root.
pub const SYNTHETIC_DIR: &str = "tests/corpus";

/// Bench-corpus manifest, relative to the workspace root.
pub const MANIFEST_REL: &str = "crates/zpdf-benches/corpus-manifest.tsv";

/// How many pages per load class the single-page latency set draws.
///
/// Three gives up to nine pages when all three classes are represented (the
/// measured corpus has text, vector and image documents), which is the 8–10 page
/// size the matrix was sized for. Two keeps a class from being characterised by
/// one possibly-atypical page while staying inside the runtime budget; override
/// with `ZPDF_BENCH_PER_CLASS` for a faster loop.
pub fn pages_per_class() -> usize {
    std::env::var("ZPDF_BENCH_PER_CLASS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(3)
}

/// One page to measure. `page` is 0-based, matching `PdfDocument::page`.
#[derive(Debug, Clone)]
pub struct PageRef {
    /// Stable criterion label, e.g. `test8-p0`.
    pub label: String,
    /// Absolute path.
    pub path: PathBuf,
    /// 0-based page index.
    pub page: usize,
    /// Measured load class, when the manifest has been classified.
    pub class: Option<String>,
}

/// One multi-page document used by the throughput / parallel-scaling benches.
#[derive(Debug, Clone)]
pub struct DocRef {
    pub label: String,
    pub path: PathBuf,
    /// Total page count, from the manifest (this is what makes the batch bench
    /// able to pick a page slice without opening the document first).
    pub pages: u32,
}

/// A loaded, verified corpus.
pub struct Corpus {
    pub root: PathBuf,
    pub manifest: Manifest,
    pub verified: Verified,
    /// True when running without the real corpus on purpose.
    pub synthetic_only: bool,
}

impl Corpus {
    /// A corpus that needs only the in-repo synthetic fixtures — no manifest, no
    /// verification, no real PDFs.
    ///
    /// This is what makes CI possible: the synthetic corpus is in version
    /// control, so a fresh clone and a CI runner both have it. Keeping this
    /// constructor free of the manifest means a CI job cannot accidentally
    /// depend on files that are deliberately absent from the repository.
    pub fn synthetic_only() -> Self {
        let root = workspace_root();
        Self {
            manifest: Manifest {
                entries: Vec::new(),
                source: root.join(MANIFEST_REL),
            },
            verified: Verified {
                entries: Vec::new(),
                bytes_hashed: 0,
            },
            root,
            synthetic_only: true,
        }
    }

    /// Verify the manifest and return the corpus, or explain exactly what is
    /// wrong. `ZPDF_BENCH_SYNTHETIC_ONLY=1` downgrades a missing real corpus to
    /// a warning; anything else is an error.
    pub fn load() -> Result<Self, String> {
        let root = workspace_root();
        let source = root.join(MANIFEST_REL);
        if synthetic_only() {
            let manifest = Manifest {
                entries: Vec::new(),
                source: source.clone(),
            };
            eprintln!(
                "zpdf-benches: ZPDF_BENCH_SYNTHETIC_ONLY=1 — real corpus ignored; \
                 measuring the synthetic corpus only. Numbers are NOT comparable \
                 to a full-corpus baseline."
            );
            return Ok(Self {
                root,
                manifest,
                verified: Verified {
                    entries: Vec::new(),
                    bytes_hashed: 0,
                },
                synthetic_only: true,
            });
        }
        if !source.exists() {
            return Err(format!(
                "bench corpus manifest {} is missing — regenerate it with:\n  \
                 cargo run -p zpdf-benches --bin corpus-manifest -- --update",
                source.display()
            ));
        }
        let manifest = Manifest::load(&source)?;
        // Hashes by default (see manifest.rs); `ZPDF_BENCH_VERIFY=size` skips
        // them for a fast local loop at the cost of content identity.
        let mode = match std::env::var("ZPDF_BENCH_VERIFY").as_deref() {
            Ok("size") => VerifyMode::SizeOnly,
            _ => VerifyMode::Strict,
        };
        let verified = manifest.verify(&root, mode)?;
        Ok(Self {
            root,
            manifest,
            verified,
            synthetic_only: false,
        })
    }

    /// Pages for the single-page latency set: at most [`pages_per_class`] per
    /// measured load class, in manifest order.
    ///
    /// Ordering falls out of the manifest, which is sorted by path — so the
    /// selection is stable across runs (a benchmark label must not silently
    /// point at a different page between two runs).
    ///
    /// While the manifest is unclassified (`class` empty) every entry shares the
    /// one class, so this falls back to [`DEFAULT_LATENCY_PAGES`] — the six
    /// pages the recorded baseline used, kept so before/after stays comparable
    /// until classification replaces them.
    pub fn latency_pages(&self) -> Vec<PageRef> {
        if self.synthetic_only {
            return Vec::new();
        }
        let classified = self.manifest.entries.iter().any(|e| e.class.is_some());
        if !classified {
            return DEFAULT_LATENCY_PAGES
                .iter()
                .filter_map(|(label, rel, page)| {
                    let path = self.root.join(rel);
                    path.exists().then(|| PageRef {
                        label: (*label).to_string(),
                        path,
                        page: *page,
                        class: None,
                    })
                })
                .collect();
        }

        // Group candidates by class, preserving manifest (path) order within a
        // group; the group order is the order classes first appear.
        let mut groups: Vec<(String, Vec<&Entry>)> = Vec::new();
        for e in &self.manifest.entries {
            let Some(class) = e.class.as_deref() else {
                continue;
            };
            // Skip pages that paint nothing: a blank cover's class is an honest
            // label but a useless latency representative, and the matrix budget
            // is spent better on pages that do work.
            if class == LoadClass::Empty.as_str() {
                continue;
            }
            match groups.iter_mut().find(|(c, _)| c == class) {
                Some((_, list)) => list.push(e),
                None => groups.push((class.to_string(), vec![e])),
            }
        }

        let per_class = pages_per_class();
        let mut out = Vec::new();
        for (class, candidates) in groups {
            let mut picked: Vec<&Entry> = Vec::new();
            // The heaviest page of the class first. Ordering by path alone would
            // drop the corpus's most expensive page (its image class has six
            // candidates and the 29 Mpx one sits fifth by path), and file size is
            // not a proxy — a 5.9 MB file holds that page while a 145 MB one
            // holds a 4 Mpx page.
            if let Some(heaviest) = candidates.iter().max_by_key(|e| e.coverage_px.unwrap_or(0)) {
                picked.push(heaviest);
            }
            // Then breadth, in manifest order: a class should not be represented
            // only by its most extreme member.
            for e in &candidates {
                if picked.len() >= per_class {
                    break;
                }
                if !picked.iter().any(|p| p.path == e.path) {
                    picked.push(e);
                }
            }
            for e in picked {
                out.push(PageRef {
                    label: label_for(&e.path),
                    path: self.root.join(&e.path),
                    page: 0,
                    class: Some(class.clone()),
                });
            }
        }
        out
    }

    /// Multi-page documents for the throughput / parallel-scaling benches,
    /// largest first, capped at [`BATCH_DOCS`].
    ///
    /// Requires manifest page counts, so it is empty under
    /// `ZPDF_BENCH_SYNTHETIC_ONLY`.
    pub fn batch_docs(&self) -> Vec<DocRef> {
        select_batch_docs(&self.manifest.entries, &self.root)
    }

    /// Every synthetic page (in-repo, so always available). One page per file —
    /// each fixture isolates a single feature, so page 0 is the whole story.
    pub fn synthetic_pages(&self) -> Vec<PageRef> {
        let dir = self.root.join(SYNTHETIC_DIR);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")))
            .collect();
        files.sort();
        files
            .into_iter()
            .map(|path| PageRef {
                // The file stem is the fixture's feature name (`rect_fills`,
                // `text_type3`, …) — cleaner as a criterion label than the
                // path-derived form.
                label: path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "synthetic".to_string()),
                path,
                page: 0,
                class: Some("synthetic".to_string()),
            })
            .collect()
    }

    /// Human-readable one-liner for startup logging.
    pub fn summary(&self) -> String {
        if self.synthetic_only {
            return format!("synthetic-only ({} pages)", self.synthetic_pages().len());
        }
        format!(
            "{} real entries verified ({:.1} MiB hashed), {} latency pages, {} batch docs",
            self.verified.entries.len(),
            self.verified.bytes_hashed as f64 / (1024.0 * 1024.0),
            self.latency_pages().len(),
            self.batch_docs().len(),
        )
    }
}

/// Pick the batch set from manifest entries: drop anything too short to show
/// per-page amortisation, sort largest-first, cap the count.
///
/// Split out from [`Corpus::batch_docs`] as a pure function so the policy is
/// testable without a filesystem or a manifest file.
fn select_batch_docs(entries: &[Entry], root: &Path) -> Vec<DocRef> {
    let mut docs: Vec<DocRef> = entries
        .iter()
        .filter_map(|e| {
            let pages = e.pages?;
            (pages >= MIN_BATCH_PAGES).then(|| DocRef {
                label: label_for(&e.path),
                path: root.join(&e.path),
                pages,
            })
        })
        .collect();
    docs.sort_by(|a, b| b.pages.cmp(&a.pages).then(a.label.cmp(&b.label)));
    docs.truncate(BATCH_DOCS);
    docs
}

/// Whether the operator explicitly opted into a real-corpus-free run.
pub fn synthetic_only() -> bool {
    matches!(
        std::env::var("ZPDF_BENCH_SYNTHETIC_ONLY").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// Documents shorter than this are useless for a throughput curve (there is no
/// per-page amortisation to observe) and are excluded from the batch set.
const MIN_BATCH_PAGES: u32 = 40;

/// How many multi-page documents the batch bench draws from.
const BATCH_DOCS: usize = 4;

/// The pages the recorded baseline used, kept as the pre-classification
/// fallback so the old numbers stay comparable. See [`Corpus::latency_pages`].
const DEFAULT_LATENCY_PAGES: &[(&str, &str, usize)] = &[
    (
        "testpdf-ai",
        "tests/testpdf/\u{79d1}\u{5e7b}\u{7535}\u{5f71}\u{4e2d}\u{7684}AI\u{4f26}\u{7406}\u{51b2}\u{7a81}.pdf",
        0,
    ),
    ("test8", "tests/test8/1.pdf", 0),
    ("zzztest2", "tests/zzztest/2.pdf", 0),
    ("test6", "tests/test6/1.pdf", 0),
    ("test3", "tests/test3/17.pdf", 0),
    ("test10", "tests/test10/1.pdf", 0),
];

/// `tests/test8/1.pdf` -> `test8`. Used as the criterion label stem.
fn label_for(rel: &str) -> String {
    let trimmed = rel
        .trim_start_matches("tests/")
        .trim_end_matches(".pdf")
        .trim_end_matches("/1");
    trimmed.replace('/', "-")
}

/// Label stem for a corpus path (absolute or root-relative).
///
/// Kept stable across runs, because a benchmark label that silently points at a
/// different file between two runs makes before/after comparison meaningless.
pub fn label_for_path(path: &Path) -> String {
    // Prefer the workspace-relative form; fall back to the file stem plus its
    // parent directory so two `1.pdf` files never collide.
    let rel = path.strip_prefix(workspace_root()).unwrap_or(path);
    let rel = rel.to_string_lossy().replace('\\', "/");
    label_for(&rel)
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

/// Convenience for a caller that only needs the workspace root.
pub fn root() -> PathBuf {
    workspace_root()
}

/// True when `path` is under the workspace root (used to keep labels stable).
pub fn is_relative_to(path: &Path, base: &Path) -> bool {
    path.strip_prefix(base).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_for_strips_dir_and_pdf() {
        assert_eq!(label_for("tests/test8/1.pdf"), "test8");
        assert_eq!(label_for("tests/zzztest/2.pdf"), "zzztest-2");
        assert_eq!(label_for("tests/test3/17.pdf"), "test3-17");
    }

    #[test]
    fn default_latency_pages_are_unique_and_bounded() {
        let mut labels: Vec<&str> = DEFAULT_LATENCY_PAGES.iter().map(|(l, _, _)| *l).collect();
        let n = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), n, "labels must be unique");
        assert!(
            n <= 10,
            "matrix budget assumes a small latency set, got {n}"
        );
    }

    /// The batch set drops documents too short to show per-page amortisation,
    /// and caps how many documents the matrix carries. Exercised as behaviour
    /// (not as an assertion on the constant's value).
    #[test]
    fn select_batch_docs_filters_short_and_caps() {
        let mk = |path: &str, pages: Option<u32>| Entry {
            sha256: "0".repeat(64),
            size: 1,
            pages,
            class: None,
            coverage_px: None,
            path: path.to_string(),
        };
        let entries = vec![
            mk("tests/test3/17.pdf", Some(3)),
            mk("tests/test6/1.pdf", Some(39)), // one below the bar
            mk("tests/test8/1.pdf", Some(40)), // exactly at the bar
            mk("tests/test10/1.pdf", Some(73)),
            mk("tests/test13/1.pdf", Some(92)),
            mk("tests/test5/1.pdf", Some(302)),
            mk("tests/test4/1.pdf", Some(400)),
            mk("tests/zzztest/2.pdf", Some(526)),
            mk("tests/nonpdf.bin", None), // no page count -> skipped
        ];
        let docs = select_batch_docs(&entries, Path::new("/root"));
        assert_eq!(docs.len(), BATCH_DOCS);
        // Largest first.
        assert_eq!(
            docs.iter().map(|d| d.pages).collect::<Vec<_>>(),
            vec![526, 400, 302, 92]
        );
        assert!(docs.iter().all(|d| d.pages >= MIN_BATCH_PAGES));
        assert!(
            docs.iter().all(|d| d.path.starts_with("/root")),
            "paths must be rooted at the workspace root"
        );
    }

    /// With no page counts at all (synthetic-only runs) the batch set is empty
    /// rather than full of unusable one-page entries.
    #[test]
    fn select_batch_docs_empty_without_page_counts() {
        let only_synthetic = vec![Entry {
            sha256: "0".repeat(64),
            size: 1,
            pages: None,
            class: Some("synthetic".into()),
            coverage_px: None,
            path: "tests/corpus/rect_fills.pdf".into(),
        }];
        assert!(select_batch_docs(&only_synthetic, Path::new("/root")).is_empty());
    }

    /// Page selection must include the heaviest page of each class, not merely
    /// the first ones in path order — otherwise the corpus's most expensive page
    /// (which sits fifth by path in its class) never gets measured.
    #[test]
    fn latency_selection_includes_the_heaviest_page_per_class() {
        let mk = |path: &str, class: &str, cov: u64| Entry {
            sha256: "0".repeat(64),
            size: 1,
            pages: Some(10),
            class: Some(class.into()),
            coverage_px: Some(cov),
            path: path.into(),
        };
        let entries = vec![
            // Image class: the heaviest is last, and must still be picked.
            mk("tests/late-heavy.pdf", "image", 9_000_000),
            mk("tests/img-a.pdf", "image", 100),
            mk("tests/img-b.pdf", "image", 50),
            mk("tests/txt-a.pdf", "text", 10),
            mk("tests/txt-b.pdf", "text", 20),
            // A blank page: honest class, useless latency representative.
            mk("tests/blank.pdf", "empty", 0),
        ];
        let c = Corpus {
            root: PathBuf::from("/root"),
            manifest: Manifest {
                entries,
                source: PathBuf::from("test"),
            },
            verified: Verified {
                entries: Vec::new(),
                bytes_hashed: 0,
            },
            synthetic_only: false,
        };
        let pages = c.latency_pages();
        let labels: Vec<&str> = pages.iter().map(|p| p.label.as_str()).collect();
        assert!(
            labels.contains(&"late-heavy"),
            "heaviest page of its class must be selected: {labels:?}"
        );
        assert!(
            !labels.contains(&"blank"),
            "a page that paints nothing must be skipped: {labels:?}"
        );
        assert!(
            labels.contains(&"txt-a") && labels.contains(&"txt-b"),
            "breadth within a class: {labels:?}"
        );
    }
}
