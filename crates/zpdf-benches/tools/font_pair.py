#!/usr/bin/env python3
"""Alternate archived benchmark executables to control long-run host drift.

The 5% gate passes only when the paired 95% bootstrap interval is below the
limit. Identical binaries additionally calibrate both directions. Uncertain
results are not a pass. Each case keeps every raw Criterion observation.
"""

import argparse
import copy
import json
import math
import os
import random
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from font_baseline import ROOT, digest, environment, validate


def measurement_environment(snapshot_environment, scheduler=None):
    """An explicit paired-run scheduler does not rewrite archived evidence."""
    if scheduler not in (None, "default", "background"):
        raise ValueError("Unsupported measurement scheduler")
    expected = copy.deepcopy(snapshot_environment)
    if scheduler is not None:
        expected["settings"]["scheduler"] = scheduler
    return expected


def paired_interval(ratios):
    if len(ratios) < 5 or any(not math.isfinite(ratio) or ratio <= 0 for ratio in ratios):
        raise ValueError("At least five finite, positive paired ratios are required")
    logs = [math.log(ratio) for ratio in ratios]
    rng = random.Random(0)
    bootstrap = sorted(statistics.median(rng.choices(logs, k=len(logs))) for _ in range(10000))
    return (math.exp(statistics.median(logs)), math.exp(bootstrap[249]), math.exp(bootstrap[9749]))


def verdict(ratios, threshold, calibration=False):
    if not math.isfinite(threshold) or threshold < 0:
        raise ValueError("Threshold must be finite and nonnegative")
    estimate, low, high = paired_interval(ratios)
    ceiling = 1 + threshold / 100
    if low > ceiling:
        status = "regression"
    elif high <= ceiling and (not calibration or low >= 1 - threshold / 100):
        status = "pass"
    else:
        status = "inconclusive"
    return {"status": status, "ratio": estimate, "lower_95": low, "upper_95": high}


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def validate_resume(saved, template, cases):
    for key in ("before", "after", "executables", "expected_cases", "environment", "settings", "gpu", "calibration"):
        if saved.get(key) != template.get(key):
            raise ValueError(f"Resume identity differs: {key}")
    if saved.get("status") != "in_progress":
        raise ValueError("Resume requires an interrupted in-progress report")
    selected = saved.get("selected_cases")
    if selected is None and saved.get("complete"):
        selected = saved["expected_cases"]  # Legacy full-suite checkpoints.
    if selected != cases or not set(saved["cases"]).issubset(cases):
        raise ValueError("Resume case selection differs")
    for case in saved["cases"].values():
        observations = case["observations"]
        if not observations or len(observations) > template["settings"]["rounds"]:
            raise ValueError("Invalid saved observation count")
        if len(observations) < template["settings"]["rounds"] and case.get("status") != "in_progress":
            raise ValueError("An unfinished case cannot have a final status")
        for item in observations:
            values = (item["before_ns"], item["after_ns"], item["ratio"])
            if not all(math.isfinite(value) and value > 0 for value in values):
                raise ValueError("Invalid saved observation")
            if not math.isclose(item["ratio"], item["after_ns"] / item["before_ns"], rel_tol=1e-12):
                raise ValueError("Saved ratio disagrees with timings")


def recovered_pair(directory, name, executables, gpu):
    paths = [directory / str(side) / "completed.json" for side in (0, 1)]
    if not all(path.exists() for path in paths):
        return None
    values = []
    for side, path in enumerate(paths):
        item = json.loads(path.read_text())
        if (item["case"] != name or item["executable"] != str(executables[side])
                or item.get("gpu") != gpu):
            raise ValueError("Completed observation identity differs")
        value = item["median_ns"]
        if not math.isfinite(value) or value <= 0:
            raise ValueError("Invalid completed observation")
        values.append(value)
    return values


def observe(executable, name, directory, args, launcher, gpu=None):
    env = dict(os.environ, CRITERION_HOME=str(directory))
    if gpu is not None:
        env["ZPDF_BENCH_GPU_METADATA"] = str(directory / "gpu.json")
    argv = launcher + [str(executable), "--bench", "--noplot", "--exact", name,
                       "--sample-size", str(args.sample_size), "--warm-up-time", str(args.warm_up),
                       "--measurement-time", str(args.measurement), "--nresamples", "10000"]
    directory.mkdir(parents=True)
    with (directory / "process.log").open("w") as log:
        subprocess.run(argv, cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
    if gpu is not None and json.loads((directory / "gpu.json").read_text()) != gpu:
        raise ValueError("Actual GPU adapter differs from the archived snapshot")
    estimates = list(directory.glob("**/new/estimates.json"))
    if len(estimates) != 1:
        raise ValueError(f"Expected exactly one result for {name}, found {len(estimates)}")
    path = estimates[0]
    identity = json.loads(path.with_name("benchmark.json").read_text())
    if identity["full_id"] != name:
        raise ValueError(f"Wrong case measured: {identity['full_id']}")
    value = json.loads(path.read_text())["median"]["point_estimate"]
    if not math.isfinite(value) or value <= 0:
        raise ValueError(f"Invalid measurement: {name}")
    write_json(directory / "completed.json", {"case": name, "executable": str(executable),
                                             "median_ns": value, "gpu": gpu})
    return value


def run(args):
    before = json.loads(Path(args.before).read_text())
    after = json.loads(Path(args.after).read_text())
    validate(before)
    validate(after)
    if before["environment"] != after["environment"] or before["inputs"] != after["inputs"]:
        raise ValueError("Snapshots must have the same environment and workload identity")
    if before.get("gpu") != after.get("gpu"):
        raise ValueError("Snapshots use different GPU adapters")
    if set(before["expected_cases"]) != set(after["expected_cases"]):
        raise ValueError("Snapshot case inventories differ")
    if args.rounds < 5 or args.sample_size < 10 or args.warm_up <= 0 or args.measurement <= 0:
        raise ValueError("Use at least five rounds, ten samples and positive measurement windows")
    output = Path(args.output).resolve()
    if output.exists():
        raise ValueError(f"Refusing to overwrite evidence: {output}")
    expected_environment = measurement_environment(before["environment"], args.scheduler)
    scheduler = expected_environment["settings"].get("scheduler", "default")
    if scheduler == "background" and sys.platform != "darwin":
        raise ValueError("Background snapshots require macOS taskpolicy")
    current = environment(expected_environment["settings"])
    if current != expected_environment:
        raise ValueError("Current host/toolchain differs from the archived snapshots")
    executables = []
    for snapshot in [before, after]:
        path = Path(snapshot["raw_results"]) / snapshot.get("executable_name", "fonts.exe" if os.name == "nt" else "fonts")
        if digest(path) != snapshot.get("benchmark_executable_sha256"):
            raise ValueError(f"Archived executable hash mismatch: {path}")
        executables.append(path)
    cases = sorted(before["expected_cases"])
    if args.case:
        unknown = set(args.case) - set(cases)
        if unknown:
            raise ValueError(f"Unknown cases: {sorted(unknown)}")
        cases = [name for name in cases if name in args.case]
    calibration = before["benchmark_executable_sha256"] == after["benchmark_executable_sha256"]
    parent = ROOT / "target" / "font-benchmark-pairs"
    parent.mkdir(parents=True, exist_ok=True)
    directory = None if args.resume else Path(tempfile.mkdtemp(prefix="pair-", dir=parent))
    launcher = ["taskpolicy", "-b"] if scheduler == "background" else []
    report = {
        "schema": 1, "before": str(Path(args.before).resolve()), "after": str(Path(args.after).resolve()),
        "calibration": calibration, "complete": len(cases) == len(before["expected_cases"]),
        "expected_cases": sorted(before["expected_cases"]), "environment": current,
        "selected_cases": cases,
        "settings": {"rounds": args.rounds, "sample_size": args.sample_size,
                     "warm_up_s": args.warm_up, "measurement_s": args.measurement,
                     "threshold_percent": args.threshold, "order": "AB, BA alternating"},
        "raw_results": str(directory), "cases": {},
        "executables": [snapshot["benchmark_executable_sha256"] for snapshot in [before, after]],
    }
    if before.get("gpu") is not None:
        report["gpu"] = before["gpu"]
    if args.scheduler is not None:
        report["snapshot_environment"] = before["environment"]
        report["settings"]["scheduler_override"] = args.scheduler
    if args.resume:
        resume_path = Path(args.resume).resolve()
        saved = json.loads(resume_path.read_text())
        validate_resume(saved, report, cases)
        directory = Path(saved["raw_results"]).resolve()
        if not directory.is_dir() or directory.parent != parent.resolve():
            raise ValueError("Resume raw directory is outside the benchmark archive")
        count = len(saved.get("resumes", [])) + 1
        archive = directory / f"resume-input-{count}.json"
        if archive.exists():
            raise ValueError("Resume input archive already exists")
        write_json(archive, saved)
        report = saved
        report["selected_cases"] = cases
        report.setdefault("resumes", []).append({"input": str(archive), "started_unix": time.time()})
        retained = sum(len(case["observations"]) for case in report["cases"].values())
        print(f"Resuming with {retained} validated paired observations retained", flush=True)
    report["status"] = "in_progress"
    write_json(directory / "progress.json", report)
    for index, name in enumerate(cases):
        observations = report["cases"].get(name, {}).get("observations", [])
        if len(observations) == args.rounds:
            result = verdict([item["ratio"] for item in observations], args.threshold, calibration)
            result["observations"] = observations
            report["cases"][name] = result
            continue
        for round_index in range(len(observations), args.rounds):
            base = directory / str(index) / str(round_index)
            attempts = [base] + sorted(base.parent.glob(f"{round_index}-retry-*"))
            recovered = [(path, recovered_pair(path, name, executables, before.get("gpu"))) for path in attempts]
            recovered = [(path, values) for path, values in recovered if values is not None]
            if len(recovered) > 1:
                raise ValueError("Multiple completed attempts exist for one pair")
            if recovered:
                pair_directory, values = recovered[0]
            else:
                pair_directory = base
                retry = 0
                while pair_directory.exists():
                    report.setdefault("unverified_attempts", [])
                    if str(pair_directory) not in report["unverified_attempts"]:
                        report["unverified_attempts"].append(str(pair_directory))
                    retry += 1
                    pair_directory = base.with_name(f"{round_index}-retry-{retry}")
                values = [None, None]
                for side in ([0, 1] if round_index % 2 == 0 else [1, 0]):
                    values[side] = observe(executables[side], name,
                                           pair_directory / str(side), args, launcher, before.get("gpu"))
            observations.append({"before_ns": values[0], "after_ns": values[1],
                                 "ratio": values[1] / values[0], "raw_pair_directory": str(pair_directory)})
            report["cases"][name] = {"status": "in_progress", "observations": observations}
            report["status"] = "in_progress"
            write_json(directory / "progress.json", report)
        result = verdict([item["ratio"] for item in observations], args.threshold, calibration)
        result["observations"] = observations
        report["cases"][name] = result
        print(f"{index + 1}/{len(cases)} {name}: {result['status']} "
              f"{(result['ratio'] - 1) * 100:+.2f}% "
              f"[95%: {(result['lower_95'] - 1) * 100:+.2f}%, "
              f"{(result['upper_95'] - 1) * 100:+.2f}%]", flush=True)
        # Keep partial evidence if a later process fails. Its explicit status
        # prevents a partial run from being mistaken for a complete pass.
        report["status"] = "in_progress"
        write_json(directory / "progress.json", report)
    statuses = [item["status"] for item in report["cases"].values()]
    passed = report["complete"] and all(status == "pass" for status in statuses)
    report["status"] = "pass" if passed else "not_accepted"
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(f"Recorded {len(cases)} paired cases to {output}; status: {report['status']}")
    return 0 if passed else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before")
    parser.add_argument("after")
    parser.add_argument("--output", required=True)
    parser.add_argument("--rounds", type=int, default=7)
    parser.add_argument("--sample-size", type=int, default=20)
    parser.add_argument("--warm-up", type=float, default=0.15)
    parser.add_argument("--measurement", type=float, default=0.4)
    parser.add_argument("--threshold", type=float, default=5.0)
    parser.add_argument("--scheduler", choices=["default", "background"],
                        help="Explicitly use this scheduler for both archived binaries; retain snapshot metadata")
    parser.add_argument("--case", action="append", help="Diagnostic subset; cannot yield a full-suite pass")
    parser.add_argument("--resume", help="Resume a stopped run's progress.json without replacing completed pairs")
    try:
        return run(parser.parse_args())
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        print(f"font-pair: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
