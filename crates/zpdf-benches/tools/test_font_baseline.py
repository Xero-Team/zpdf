"""The font gate must fail closed when evidence is missing or incomparable."""

import copy
import io
import unittest
from contextlib import redirect_stdout

from font_baseline import compare, validate
from font_pair import verdict


class FontBaselineTests(unittest.TestCase):
    def setUp(self):
        self.snapshot = {"schema": 1, "expected_cases": ["font/a", "font/b"],
                         "environment": {"cpu": "test-cpu"}, "inputs": {"font.ttf": "hash"},
                         "cases": {name: {"median_ns": 100.0, "mean_ns": 100.0}
                                   for name in ["font/a", "font/b"]}}

    def test_identical_runs_pass(self):
        with redirect_stdout(io.StringIO()):
            self.assertTrue(compare(self.snapshot, self.snapshot, 5.0))

    def test_regression_fails(self):
        after = copy.deepcopy(self.snapshot)
        after["cases"]["font/a"]["median_ns"] = 106.0
        with redirect_stdout(io.StringIO()):
            self.assertFalse(compare(self.snapshot, after, 5.0))

    def test_missing_and_empty_results_fail(self):
        for cases in [{}, {"font/a": self.snapshot["cases"]["font/a"]}]:
            after = copy.deepcopy(self.snapshot)
            after["cases"] = cases
            with self.assertRaises(ValueError):
                validate(after)

    def test_changed_environment_or_workload_fails(self):
        for field in ["environment", "inputs"]:
            after = copy.deepcopy(self.snapshot)
            after[field]["changed"] = True
            with self.assertRaises(ValueError):
                compare(self.snapshot, after, 5.0)

    def test_removed_case_cannot_redefine_success(self):
        after = copy.deepcopy(self.snapshot)
        after["expected_cases"].remove("font/b")
        del after["cases"]["font/b"]
        with self.assertRaises(ValueError):
            compare(self.snapshot, after, 5.0)

    def test_invalid_measurements_and_thresholds_fail(self):
        for value in [0, -1, float("nan"), float("inf")]:
            after = copy.deepcopy(self.snapshot)
            after["cases"]["font/a"]["median_ns"] = value
            with self.assertRaises(ValueError):
                validate(after)
        for value in [-1, float("nan"), float("inf")]:
            with self.assertRaises(ValueError):
                compare(self.snapshot, self.snapshot, value)

    def test_paired_gate_separates_regression_from_uncertainty(self):
        self.assertEqual(verdict([1.0] * 7, 5.0, True)["status"], "pass")
        self.assertEqual(verdict([1.07] * 7, 5.0)["status"], "regression")
        self.assertEqual(verdict([0.8, 0.9, 0.95, 1.0, 1.05, 1.1, 1.2], 5.0)["status"], "inconclusive")

    def test_calibration_rejects_unexplained_speedups(self):
        self.assertEqual(verdict([0.8] * 7, 5.0, True)["status"], "inconclusive")
        self.assertEqual(verdict([0.8] * 7, 5.0)["status"], "pass")

    def test_paired_gate_rejects_insufficient_or_invalid_samples(self):
        for ratios in [[1.0] * 4, [float("nan")] * 7, [0] * 7]:
            with self.assertRaises(ValueError):
                verdict(ratios, 5.0)

    def test_executable_capture_cannot_claim_a_timing_pass(self):
        capture = copy.deepcopy(self.snapshot)
        capture.update(kind="executable", cases={}, benchmark_executable_sha256="binary-hash")
        validate(capture)
        with self.assertRaises(ValueError):
            compare(capture, capture, 5.0)
        capture["cases"] = self.snapshot["cases"]
        with self.assertRaises(ValueError):
            validate(capture)

    def test_gpu_identity_is_required_and_cannot_change(self):
        before = copy.deepcopy(self.snapshot)
        before["environment"]["settings"] = {"benchmark_target": "font_gpu"}
        with self.assertRaises(ValueError):
            validate(before)
        before["gpu"] = {"fingerprint": "adapter-A"}
        validate(before)
        after = copy.deepcopy(before)
        after["gpu"] = {"fingerprint": "adapter-B"}
        with self.assertRaises(ValueError):
            compare(before, after, 5.0)


if __name__ == "__main__":
    unittest.main()
