#!/usr/bin/env python3
"""Run portable font benchmarks and compare complete same-host measurements.

Unlike scanning a shared target/criterion directory, each run owns a fresh
results directory. A failed/partial run never produces an accepted snapshot.
"""

import argparse
import hashlib
import json
import math
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
INPUTS = [
    "crates/zpdf-font/tests/fixtures/coverage-primary.ttf",
    "crates/zpdf-font/tests/fixtures/coverage-fallback.ttf",
    "crates/zpdf-font/tests/fixtures/var.ttf",
    "crates/zpdf/tests/fixtures/quartz_cff_subset.pdf",
    "crates/zpdf-benches/src/font_workloads.rs",
    "crates/zpdf-benches/benches/fonts.rs",
]


def command(args, **kwargs):
    return subprocess.check_output(args, cwd=ROOT, text=True, **kwargs).strip()


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_fingerprint():
    """Include untracked implementation files, not just the tracked diff."""
    paths = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard",
         "--", "Cargo.toml", "crates"], cwd=ROOT).split(b"\0")
    result = hashlib.sha256()
    for raw in sorted(set(path for path in paths if path)):
        path = ROOT / os.fsdecode(raw)
        if "__pycache__" in path.parts:
            continue
        result.update(raw + b"\0")
        result.update(path.read_bytes() if path.is_file() else b"<deleted>")
    return result.hexdigest()


def environment(settings):
    cpu = platform.processor()
    if sys.platform == "darwin":
        cpu = command(["sysctl", "-n", "machdep.cpu.brand_string"])
    elif Path("/proc/cpuinfo").exists():
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                cpu = line.split(":", 1)[1].strip()
                break
    return {
        "cpu": cpu or os.environ.get("PROCESSOR_IDENTIFIER", "unknown"),
        "logical_cpus": os.cpu_count(),
        "platform": platform.platform(),
        "architecture": platform.machine(),
        "rustc": command(["rustc", "--version", "--verbose"]),
        "rustflags": os.environ.get("RUSTFLAGS", ""),
        "encoded_rustflags": os.environ.get("CARGO_ENCODED_RUSTFLAGS", ""),
        "profile": "bench/release",
        "settings": settings,
    }


def finite_positive(value):
    return isinstance(value, (int, float)) and math.isfinite(value) and value > 0


def validate(snapshot):
    if snapshot.get("schema") != 1:
        raise ValueError("Unsupported font baseline schema")
    cases = snapshot.get("cases", {})
    expected = snapshot.get("expected_cases", [])
    if not expected or len(expected) != len(set(expected)):
        raise ValueError("Empty or duplicate benchmark inventory")
    if not snapshot.get("environment") or not snapshot.get("inputs"):
        raise ValueError("Missing environment or workload identity")
    if snapshot["environment"].get("settings", {}).get("benchmark_target") == "font_gpu":
        if not snapshot.get("gpu", {}).get("fingerprint"):
            raise ValueError("Missing actual GPU adapter identity")
    if snapshot.get("kind") == "executable":
        if cases or not snapshot.get("benchmark_executable_sha256"):
            raise ValueError("An executable capture must contain a binary identity and no timings")
        return
    if not cases or set(cases) != set(expected):
        raise ValueError("Incomplete benchmark results")
    for name, case in cases.items():
        if not finite_positive(case.get("median_ns")) or not finite_positive(case.get("mean_ns")):
            raise ValueError(f"Invalid timing for {name}")


def compare(before, after, threshold):
    validate(before)
    validate(after)
    if before.get("kind") == "executable" or after.get("kind") == "executable":
        raise ValueError("Executable captures have no timings; use the paired measurement runner")
    if not math.isfinite(threshold) or threshold < 0:
        raise ValueError("Threshold must be finite and nonnegative")
    if before["environment"] != after["environment"]:
        raise ValueError("Environment mismatch: these runs cannot establish a regression pass")
    if before.get("gpu") != after.get("gpu"):
        raise ValueError("GPU adapter mismatch")
    if before["inputs"] != after["inputs"]:
        raise ValueError("Workload mismatch: record the extended suite before production changes")
    if set(before["cases"]) != set(after["cases"]):
        raise ValueError("Case inventory changed: missing/new cases require an explicit new baseline")
    rows = []
    for name, case in before["cases"].items():
        old, new = case["median_ns"], after["cases"][name]["median_ns"]
        rows.append((name, old, new, (new / old - 1) * 100))
    rows.sort(key=lambda row: row[3], reverse=True)
    for name, old, new, delta in rows:
        mark = " REGRESSION" if delta > threshold else ""
        print(f"{name:70} {old / 1000:10.2f} -> {new / 1000:10.2f} us {delta:+7.2f}%{mark}")
    failures = [row for row in rows if row[3] > threshold]
    print(f"Compared all {len(rows)} cases; {len(failures)} exceed +{threshold:g}%.")
    return not failures


def run(args):
    if args.sample_size < 10 or args.warm_up <= 0 or args.measurement <= 0:
        raise ValueError("Use at least 10 samples and positive timing windows")
    if args.scheduler == "background" and sys.platform != "darwin":
        raise ValueError("The background scheduler diagnostic currently requires macOS taskpolicy")
    settings = {"sample_size": args.sample_size, "warm_up_s": args.warm_up,
                "measurement_s": args.measurement, "scheduler": args.scheduler}
    if args.bench != "fonts":
        settings["benchmark_target"] = args.bench
    output = Path(args.output).resolve()
    if output.exists():
        raise ValueError(f"Refusing to overwrite evidence: {output}")
    input_paths = INPUTS if args.bench == "fonts" else INPUTS[:-1] + [
        f"crates/zpdf-benches/benches/{args.bench}.rs"]
    if args.bench == "font_diagnostics":
        input_paths = input_paths + ["crates/zpdf-font/tests/fixtures/coverage-type1.pfa"]
    if args.bench in ("font_packs", "font_coverage", "font_fallback", "font_writer_fallback", "font_gpu"):
        input_paths = input_paths + ["crates/zpdf-font/tests/fixtures/noto-sans-sc-subset.ttf",
                                     "crates/zpdf-benches/src/font_pack_workloads.rs"]
    if args.bench == "font_coverage":
        input_paths = input_paths + ["crates/zpdf-font/tests/fixtures/coverage-uvs.ttf"]
    if args.bench == "font_cmaps":
        input_paths = input_paths + ["crates/zpdf-font/tests/fixtures/coverage-cmap.ttf",
                                     "crates/zpdf-font/tests/fixtures/coverage-cid.otf",
                                     "crates/zpdf-benches/src/font_cmap_workloads.rs"]
    if args.bench in ("font_fallback", "font_gpu"):
        input_paths = input_paths + ["crates/zpdf-benches/src/font_fallback_workloads.rs"]
    inputs = {name: digest(ROOT / name) for name in input_paths}
    source = source_fingerprint()
    subprocess.run([sys.executable, "crates/zpdf-font/tests/fixtures/generate_coverage_fonts.py", "--check"],
                   cwd=ROOT, check=True)
    subprocess.run(["cargo", "test", "--release", "--locked", "-p", "zpdf-benches",
                    "--test", "font_workloads"], cwd=ROOT, check=True)
    if args.bench == "font_provider":
        subprocess.run(["cargo", "test", "--release", "--locked", "-p", "zpdf-font", "--test", "provider",
                        "-p", "zpdf-content", "--test", "font_provider"], cwd=ROOT, check=True)
    if args.bench == "font_packs":
        subprocess.run(["cargo", "test", "--release", "--locked", "-p", "zpdf-font", "--test", "packs",
                        "-p", "zpdf-benches", "--test", "font_packs"], cwd=ROOT, check=True)
    if args.bench == "font_coverage":
        subprocess.run(["cargo", "test", "--release", "--locked", "-p", "zpdf-font", "--test", "coverage"],
                       cwd=ROOT, check=True)
    if args.bench in ("font_fallback", "font_writer_fallback"):
        subprocess.run(["cargo", "test", "--release", "--locked", "-p", "zpdf-benches", "--test", args.bench],
                       cwd=ROOT, check=True)
    if args.bench == "font_cmaps":
        subprocess.run(["cargo", "test", "--release", "--locked", "-p", "zpdf-benches", "--test", "font_cmaps",
                        "-p", "zpdf-font", "--test", "adobe_cmaps"], cwd=ROOT, check=True)
    features = ["--features", "gpu-render"] if args.bench == "font_gpu" else []
    if features:
        subprocess.run(["cargo", "test", "--release", "--locked", "-p", "zpdf-benches",
                        "--test", "font_fallback"] + features, cwd=ROOT, check=True)
    directory = ROOT / "target" / "font-benchmark-runs"
    directory.mkdir(parents=True, exist_ok=True)
    results = Path(tempfile.mkdtemp(prefix="run-", dir=directory))
    env = dict(os.environ, CRITERION_HOME=str(results))
    if features:
        env["ZPDF_BENCH_GPU_METADATA"] = str(results / "gpu.json")
    artifacts = command(["cargo", "bench", "--locked", "-p", "zpdf-benches",
                         "--bench", args.bench, "--no-run", "--message-format=json"] + features)
    executables = [item["executable"] for line in artifacts.splitlines()
                   if (item := json.loads(line)).get("reason") == "compiler-artifact"
                   and item.get("target", {}).get("name") == args.bench and item.get("executable")]
    if len(executables) != 1:
        raise ValueError("Expected exactly one compiled font benchmark executable")
    executable = results / "executable" / (args.bench + (".exe" if os.name == "nt" else ""))
    executable.parent.mkdir()
    shutil.copy2(executables[0], executable)
    launcher = ["taskpolicy", "-b"] if args.scheduler == "background" else []
    bench = launcher + [str(executable), "--bench"]
    listing = command(bench + ["--list"], env=env)
    gpu = json.loads((results / "gpu.json").read_text()) if features else None
    expected = sorted(line.removesuffix(": benchmark") for line in listing.splitlines()
                      if line.endswith(": benchmark"))
    if not expected:
        raise ValueError(f"Benchmark inventory is empty:\n{listing}")
    started = time.time()
    if args.mode == "run":
        subprocess.run(bench + ["--noplot", "--sample-size", str(args.sample_size),
                                "--warm-up-time", str(args.warm_up),
                                "--measurement-time", str(args.measurement)],
                       cwd=ROOT, env=env, check=True)
    cases = {}
    for estimate_path in results.glob("**/new/estimates.json"):
        identity = json.loads(estimate_path.with_name("benchmark.json").read_text())
        estimates = json.loads(estimate_path.read_text())
        name = identity["full_id"]
        if name in cases:
            raise ValueError(f"Duplicate benchmark result: {name}")
        cases[name] = {
            "median_ns": estimates["median"]["point_estimate"],
            "mean_ns": estimates["mean"]["point_estimate"],
            "median_confidence_interval": estimates["median"]["confidence_interval"],
        }
    if inputs != {name: digest(ROOT / name) for name in input_paths} or source != source_fingerprint():
        raise ValueError("Sources or workloads changed during measurement; results are not a baseline")
    if features and gpu != json.loads((results / "gpu.json").read_text()):
        raise ValueError("GPU adapter changed during measurement")
    snapshot = {
        "schema": 1,
        "kind": "measurement" if args.mode == "run" else "executable",
        "started_unix": started,
        "finished_unix": time.time(),
        "environment": environment(settings),
        "git_commit": command(["git", "rev-parse", "HEAD"]),
        "git_diff_sha256": hashlib.sha256(subprocess.check_output(
            ["git", "diff", "--binary", "HEAD"], cwd=ROOT)).hexdigest(),
        "source_tree_sha256": source,
        "benchmark_executable_sha256": digest(executable),
        "executable_name": str(executable.relative_to(results)),
        "cargo_lock_sha256": digest(ROOT / "Cargo.lock"),
        "inputs": inputs,
        "expected_cases": expected,
        "cases": cases,
        "raw_results": str(results),
        "scope": "Portable font suite; not the missing historical private PDF corpus",
    }
    if features:
        snapshot["gpu"] = gpu
    validate(snapshot)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(snapshot, indent=2, sort_keys=True) + "\n")
    if args.mode == "run":
        print(f"Recorded all {len(cases)} cases to {output}")
    else:
        print(f"Captured executable and {len(expected)} case identities to {output}; no timing claim")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="mode", required=True)
    for mode in ["run", "capture"]:
        record = subparsers.add_parser(mode)
        record.add_argument("--output", required=True)
        record.add_argument("--sample-size", type=int, default=50)
        record.add_argument("--warm-up", type=float, default=1.0)
        record.add_argument("--measurement", type=float, default=2.0)
        record.add_argument("--scheduler", choices=["default", "background"], default="default")
        record.add_argument("--bench", choices=["fonts", "font_provider", "font_diagnostics", "font_packs", "font_coverage", "font_fallback", "font_writer_fallback", "font_gpu", "font_cmaps"], default="fonts")
    comparison = subparsers.add_parser("compare")
    comparison.add_argument("before")
    comparison.add_argument("after")
    comparison.add_argument("--threshold", type=float, default=5.0)
    args = parser.parse_args()
    try:
        if args.mode in ("run", "capture"):
            run(args)
        elif not compare(json.loads(Path(args.before).read_text()),
                         json.loads(Path(args.after).read_text()), args.threshold):
            return 1
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        print(f"font-baseline: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
