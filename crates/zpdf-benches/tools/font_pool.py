#!/usr/bin/env python3
"""Pool completed paired runs without discarding any earlier observations.

A fixed extension for uncertain cases can join a full run. Executables,
inventories, environment and actual measurement windows must match. Every
observation is retained; incomplete inventories never establish acceptance.
"""

import argparse
import copy
import json
import math
from pathlib import Path

from font_pair import verdict


def pool(reports):
    if not reports:
        raise ValueError("At least one completed report is required")
    first = reports[0]
    identity = ("before", "after", "executables", "expected_cases", "environment", "calibration", "gpu")
    protocol = {key: value for key, value in first["settings"].items() if key != "rounds"}
    result = {key: copy.deepcopy(first[key]) for key in identity if key in first}
    result.update(schema=1, kind="pooled_pairs", settings=protocol, cases={}, sources=[])
    directories = set()
    expected = set(first["expected_cases"])
    if not expected or len(expected) != len(first["expected_cases"]):
        raise ValueError("Invalid expected case inventory")
    for report in reports:
        if report.get("status") not in ("pass", "not_accepted"):
            raise ValueError("Only completed paired runs can be pooled")
        if any(report.get(key) != first.get(key) for key in identity):
            raise ValueError("Paired run identities differ")
        if {key: value for key, value in report["settings"].items() if key != "rounds"} != protocol:
            raise ValueError("Measurement protocols differ")
        directory = report.get("raw_results")
        if not directory or directory in directories:
            raise ValueError("Missing or repeated raw observation directory")
        directories.add(directory)
        result["sources"].append({"raw_results": directory, "rounds": report["settings"]["rounds"]})
        if not report["cases"] or not set(report["cases"]).issubset(expected):
            raise ValueError("Unexpected or empty case inventory")
        for name, case in report["cases"].items():
            observations = case["observations"]
            if len(observations) != report["settings"]["rounds"]:
                raise ValueError("Reported round count does not match observations")
            for item in observations:
                before, after = item["before_ns"], item["after_ns"]
                if not all(math.isfinite(value) and value > 0 for value in (before, after, item["ratio"])):
                    raise ValueError("Invalid paired observation")
                if not math.isclose(item["ratio"], after / before, rel_tol=1e-12):
                    raise ValueError("Ratio does not match raw timings")
            result["cases"].setdefault(name, {"observations": []})["observations"].extend(copy.deepcopy(observations))
    for case in result["cases"].values():
        case.update(verdict([item["ratio"] for item in case["observations"]],
                            protocol["threshold_percent"], first.get("calibration", False)))
    result["complete"] = set(result["cases"]) == expected
    result["status"] = "pass" if result["complete"] and all(case["status"] == "pass" for case in result["cases"].values()) else "not_accepted"
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reports", nargs="+")
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    output = Path(args.output)
    if output.exists():
        parser.error("Refusing to overwrite evidence")
    report = pool([json.loads(Path(path).read_text()) for path in args.reports])
    report["source_reports"] = args.reports
    output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    counts = {status: sum(case["status"] == status for case in report["cases"].values())
              for status in ("pass", "inconclusive", "regression")}
    print(f"Pooled {len(args.reports)} completed reports: {counts}; status: {report['status']}")
    return 0 if report["status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
