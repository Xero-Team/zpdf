//! Record and compare performance baselines.
//!
//! Criterion already saves results under `target/criterion/`, but `/target` is
//! gitignored: `cargo clean` deletes them, they cannot be reviewed in a pull
//! request, and they are not portable between checkouts. So the numbers worth
//! keeping are copied into a checked-in baseline that also records **what they
//! were measured on** — because a benchmark number without its machine, its
//! corpus revision and its dependency versions is not comparable to anything,
//! including a rerun of itself.
//!
//! Two modes:
//!
//! ```text
//! cargo run --release -p zpdf-benches --bin baseline -- --record
//! cargo run --release -p zpdf-benches --bin baseline -- --compare
//! ```
//!
//! `--record` reads criterion's estimates and writes
//! `crates/zpdf-benches/baseline/baseline.json`. `--compare` reads the current
//! results, prints a per-case delta table, and **exits non-zero** when a case
//! regresses by more than `--threshold` percent (default 5).
//!
//! Deliberately not a CI gate on wall-clock: see `tests/perf_gate.rs` and
//! `.github/workflows/bench.yml` for what CI can honestly assert. Wall-clock on
//! a shared runner is noise, and a gate that cries wolf gets ignored — which is
//! worse than no gate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use zpdf_benches::manifest::sha256_file;

/// Bump when the baseline's meaning changes (a new field, a renamed case), so an
/// older file is rejected rather than silently misread.
const SCHEMA: u32 = 1;

const DEFAULT_THRESHOLD_PCT: f64 = 5.0;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let help = args.iter().any(|a| a == "--help" || a == "-h");
    let do_record = args.iter().any(|a| a == "--record");
    let do_compare = args.iter().any(|a| a == "--compare");
    let threshold = flag_value(&args, "--threshold")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(DEFAULT_THRESHOLD_PCT);

    if help || (!do_record && !do_compare) {
        println!(
            "baseline — record or compare zpdf performance baselines\n\n\
             USAGE:\n  \
             baseline --record                      write the checked-in baseline\n  \
             baseline --compare [--threshold 5.0]   compare current results to it\n\n\
             Run the benches first (release profile, so the numbers exist):\n  \
             cargo bench -p zpdf-benches --features gpu-render \\\n    \
             --bench stages --bench backend --bench batch\n\n\
             Files:\n  \
             target/criterion/...              criterion's raw results (gitignored)\n  \
             crates/zpdf-benches/baseline/     the checked-in baseline (reviewable)\n"
        );
        return;
    }

    let root = workspace_root();
    let result = if do_record {
        record(&root)
    } else {
        compare(&root, threshold)
    };
    if let Err(e) = result {
        eprintln!("baseline: {e}");
        std::process::exit(1);
    }
}

#[derive(Debug, Clone)]
struct Case {
    median_ns: f64,
    mean_ns: f64,
}

/// Where a measurement came from. Compared between runs, and *reported* rather
/// than enforced: a different machine legitimately produces different numbers,
/// and a hard failure would only train people to skip the check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Environment {
    cpu: String,
    cores: usize,
    os: String,
    arch: String,
    git_commit: String,
    corpus_manifest_sha256: String,
    corpus_entries: usize,
}

fn capture_environment(root: &Path) -> Environment {
    let manifest = root.join("crates/zpdf-benches/corpus-manifest.tsv");
    let (sha, entries) = match std::fs::read_to_string(&manifest) {
        Ok(text) => (
            sha256_file(&manifest).unwrap_or_else(|_| "unreadable".into()),
            text.lines()
                .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
                .count(),
        ),
        Err(_) => ("missing".into(), 0),
    };
    Environment {
        cpu: cpu_name(),
        cores: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        git_commit: git_commit(root),
        corpus_manifest_sha256: sha,
        corpus_entries: entries,
    }
}

/// CPU identity without pulling in a platform crate: `PROCESSOR_IDENTIFIER` on
/// Windows, the `model name` line on Linux, `unknown` elsewhere.
fn cpu_name() -> String {
    if let Ok(id) = std::env::var("PROCESSOR_IDENTIFIER") {
        if !id.trim().is_empty() {
            return id.trim().to_string();
        }
    }
    if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("model name") {
                if let Some((_, value)) = rest.split_once(':') {
                    return value.trim().to_string();
                }
            }
        }
    }
    "unknown".to_string()
}

fn git_commit(root: &Path) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// Collect every `*/new/estimates.json` under criterion's output directory.
fn collect_cases(root: &Path) -> Result<BTreeMap<String, Case>, String> {
    let dir = root.join("target/criterion");
    if !dir.is_dir() {
        return Err(format!(
            "{} not found — run the benches first:\n  cargo bench -p zpdf-benches \
             --features gpu-render --bench stages --bench backend --bench batch",
            dir.display()
        ));
    }
    let mut out = BTreeMap::new();
    walk_estimates(&dir, &dir, &mut out)?;
    if out.is_empty() {
        return Err(format!(
            "no criterion results under {} — run the benches first",
            dir.display()
        ));
    }
    Ok(out)
}

fn walk_estimates(dir: &Path, base: &Path, out: &mut BTreeMap<String, Case>) -> Result<(), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for item in rd {
        let item = item.map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
        let path = item.path();
        if path.is_dir() {
            walk_estimates(&path, base, out)?;
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) != Some("estimates.json") {
            continue;
        }
        // `.../<case>/new/estimates.json` — only the `new` (latest) run.
        if path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            != Some("new")
        {
            continue;
        }
        let Some(case_dir) = path.parent().and_then(|p| p.parent()) else {
            continue;
        };
        let Ok(rel) = case_dir.strip_prefix(base) else {
            continue;
        };
        // Criterion sanitises `/` in a benchmark id to `_` when it builds the
        // result directory names, so an id reads `stages_interpret/test8-text@96`
        // here where the report shows `stages/interpret/test8-text@96`. Stable
        // either way — comparisons match on the same spelling — but do not be
        // surprised by the leading underscore form.
        let id = rel.to_string_lossy().replace('\\', "/");
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let v: Value =
            serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
        let Some(median) = v["median"]["point_estimate"].as_f64() else {
            continue;
        };
        let mean = v["mean"]["point_estimate"].as_f64().unwrap_or(f64::NAN);
        out.insert(
            id,
            Case {
                median_ns: median,
                mean_ns: mean,
            },
        );
    }
    Ok(())
}

fn record(root: &Path) -> Result<(), String> {
    let cases = collect_cases(root)?;
    let env = capture_environment(root);
    let out_dir = root.join("crates/zpdf-benches/baseline");
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("create {}: {e}", out_dir.display()))?;
    let out = out_dir.join("baseline.json");

    let mut cases_json = serde_json::Map::new();
    for (id, case) in &cases {
        cases_json.insert(
            id.clone(),
            serde_json::json!({
                "median_ns": case.median_ns,
                "mean_ns": case.mean_ns,
            }),
        );
    }
    let doc = serde_json::json!({
        "schema": SCHEMA,
        "recorded_unix": zpdf_core::time::unix_seconds(),
        "environment": {
            "cpu": env.cpu,
            "cores": env.cores,
            "os": env.os,
            "arch": env.arch,
            "git_commit": env.git_commit,
            "corpus_manifest_sha256": env.corpus_manifest_sha256,
            "corpus_entries": env.corpus_entries,
        },
        "cases": Value::Object(cases_json),
    });
    std::fs::write(
        &out,
        serde_json::to_string_pretty(&doc).map_err(|e| format!("serialize: {e}"))? + "\n",
    )
    .map_err(|e| format!("write {}: {e}", out.display()))?;

    println!(
        "baseline: recorded {} cases to {}\n  cpu: {} ({} cores)\n  git: {}\n  corpus: {} entries, manifest {}\n  \
         NOTE: review this diff — it is the record of what counts as 'not a regression'.",
        cases.len(),
        out.display(),
        env.cpu,
        env.cores,
        env.git_commit,
        env.corpus_entries,
        &env.corpus_manifest_sha256[..12.min(env.corpus_manifest_sha256.len())],
    );
    Ok(())
}

fn compare(root: &Path, threshold_pct: f64) -> Result<(), String> {
    let baseline_path = root.join("crates/zpdf-benches/baseline/baseline.json");
    let text = std::fs::read_to_string(&baseline_path)
        .map_err(|e| format!("read {}: {e}", baseline_path.display()))?;
    let doc: Value = serde_json::from_str(&text)
        .map_err(|e| format!("parse {}: {e}", baseline_path.display()))?;

    let schema = doc["schema"].as_u64().unwrap_or(0);
    if schema != SCHEMA as u64 {
        return Err(format!(
            "baseline schema {schema} != expected {SCHEMA} — re-record it"
        ));
    }

    let current = collect_cases(root)?;
    let mut baseline_cases: BTreeMap<String, f64> = BTreeMap::new();
    if let Some(map) = doc["cases"].as_object() {
        for (id, case) in map {
            if let Some(median) = case["median_ns"].as_f64() {
                baseline_cases.insert(id.clone(), median);
            }
        }
    }

    // Environment: report differences loudly, but do not fail on them. Comparing
    // across machines is meaningless, so the numbers are shown with the mismatch
    // stated rather than silently believable.
    let base_env = &doc["environment"];
    let cur_env = capture_environment(root);
    let mut mismatches: Vec<String> = Vec::new();
    let mut note_if_diff = |name: &str, base: &str, cur: &str| {
        if base != cur {
            mismatches.push(format!("{name}: baseline {base:?} vs current {cur:?}"));
        }
    };
    note_if_diff("cpu", base_env["cpu"].as_str().unwrap_or("?"), &cur_env.cpu);
    note_if_diff("os", base_env["os"].as_str().unwrap_or("?"), &cur_env.os);
    note_if_diff(
        "arch",
        base_env["arch"].as_str().unwrap_or("?"),
        &cur_env.arch,
    );
    note_if_diff(
        "corpus manifest sha256",
        base_env["corpus_manifest_sha256"].as_str().unwrap_or("?"),
        &cur_env.corpus_manifest_sha256,
    );
    if base_env["cores"].as_u64().unwrap_or(0) as usize != cur_env.cores {
        mismatches.push(format!(
            "cores: baseline {} vs current {}",
            base_env["cores"], cur_env.cores
        ));
    }

    println!(
        "baseline: {} ({})\n  recorded at unix {} on {} ({} cores)\n",
        baseline_path.display(),
        base_env["git_commit"].as_str().unwrap_or("?"),
        doc["recorded_unix"].as_u64().unwrap_or(0),
        base_env["cpu"].as_str().unwrap_or("?"),
        base_env["cores"].as_u64().unwrap_or(0),
    );
    if !mismatches.is_empty() {
        println!("  !! ENVIRONMENT MISMATCH — deltas below are not comparable:");
        for m in &mismatches {
            println!("     {m}");
        }
        println!();
    }

    struct Row {
        id: String,
        base: f64,
        cur: f64,
        delta_pct: f64,
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut missing: Vec<&String> = Vec::new();
    for (id, base) in &baseline_cases {
        match current.get(id) {
            Some(case) if *base > 0.0 => rows.push(Row {
                id: id.clone(),
                base: *base,
                cur: case.median_ns,
                delta_pct: (case.median_ns - base) / base * 100.0,
            }),
            Some(_) => {}
            None => missing.push(id),
        }
    }
    let new_cases: Vec<&String> = current
        .keys()
        .filter(|id| !baseline_cases.contains_key(*id))
        .collect();

    rows.sort_by(|a, b| b.delta_pct.partial_cmp(&a.delta_pct).unwrap());

    println!(
        "{:<44} {:>12} {:>12} {:>9}",
        "case (median)", "baseline", "current", "delta"
    );
    for r in &rows {
        let mark = if r.delta_pct > threshold_pct {
            "  REGRESSION"
        } else if r.delta_pct < -threshold_pct {
            "  faster"
        } else {
            ""
        };
        println!(
            "{:<44} {:>10.2}ms {:>10.2}ms {:>8.1}%{}",
            truncate(&r.id, 44),
            r.base / 1e6,
            r.cur / 1e6,
            r.delta_pct,
            mark
        );
    }

    if !missing.is_empty() {
        println!(
            "\n{} case(s) in the baseline were not measured this run:",
            missing.len()
        );
        for id in &missing {
            println!("  {id}");
        }
    }
    if !new_cases.is_empty() {
        println!("\n{} new case(s) not in the baseline:", new_cases.len());
        for id in &new_cases {
            println!("  {id}");
        }
    }

    let regressions: Vec<&Row> = rows
        .iter()
        .filter(|r| r.delta_pct > threshold_pct)
        .collect();
    if !regressions.is_empty() {
        println!(
            "\n{} case(s) regressed by more than {threshold_pct:.1}%:",
            regressions.len()
        );
        for r in &regressions {
            println!(
                "  {} +{:.1}% ({:.2}ms -> {:.2}ms)",
                r.id,
                r.delta_pct,
                r.base / 1e6,
                r.cur / 1e6
            );
        }
        if !mismatches.is_empty() {
            println!(
                "\nNOTE: the environment differs from the baseline, so these may be machine \
                 differences rather than code changes. Re-record on this machine to set a new \
                 reference."
            );
        }
        return Err(format!("{} case(s) regressed", regressions.len()));
    }

    println!(
        "\nno case regressed by more than {threshold_pct:.1}% ({} compared)",
        rows.len()
    );
    Ok(())
}

/// Truncate to `max` **characters** (not bytes).
///
/// Case ids contain non-ASCII (`testpdf-科幻电影中的AI伦理冲突-image`), so byte
/// slicing panics on a char boundary — which it did, in the first version of this
/// tool.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(s.chars().count() - (max - 3)).collect();
    format!("...{tail}")
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or(manifest)
}
