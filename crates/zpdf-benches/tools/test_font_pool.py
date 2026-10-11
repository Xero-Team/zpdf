import copy
import unittest
from font_pool import pool


def report(directory, cases):
    return {"status": "not_accepted", "before": "before.json", "after": "after.json",
            "executables": ["a", "b"], "expected_cases": ["first", "second"],
            "environment": {"cpu": "test"}, "calibration": False,
            "settings": {"rounds": 5, "sample_size": 10, "measurement_s": 0.25,
                         "warm_up_s": 0.1, "threshold_percent": 5},
            "raw_results": directory,
            "cases": {name: {"observations": [{"before_ns": 100, "after_ns": value, "ratio": value / 100}
                                                for _ in range(5)]} for name, value in cases.items()}}


class PoolTests(unittest.TestCase):
    def test_extension_retains_all_original_observations_and_inventory(self):
        first = report("first", {"first": 100, "second": 101})
        second = report("extension", {"second": 100})
        result = pool([first, second])
        self.assertEqual(result["status"], "pass")
        self.assertEqual(len(result["cases"]["first"]["observations"]), 5)
        self.assertEqual(len(result["cases"]["second"]["observations"]), 10)
        self.assertEqual(len(first["cases"]["second"]["observations"]), 5)

    def test_partial_or_regressed_workload_never_passes(self):
        self.assertEqual(pool([report("partial", {"first": 90})])["status"], "not_accepted")
        self.assertEqual(pool([report("regression", {"first": 120, "second": 90})])["status"], "not_accepted")

    def test_duplicate_sources_and_different_protocols_are_rejected(self):
        first = report("original", {"first": 100, "second": 100})
        for change in [lambda r: None,
                       lambda r: r.update(executables=["a", "c"], raw_results="new"),
                       lambda r: r.update(status="in_progress", raw_results="new"),
                       lambda r: r["settings"].update(sample_size=20)]:
            second = copy.deepcopy(first)
            change(second)
            with self.assertRaises(ValueError):
                pool([first, second])

    def test_fabricated_ratio_or_missing_observation_is_rejected(self):
        first = report("original", {"first": 100, "second": 100})
        first["cases"]["first"]["observations"][0]["ratio"] = 0.5
        with self.assertRaises(ValueError):
            pool([first])
        first["cases"]["first"]["observations"].pop(0)
        with self.assertRaises(ValueError):
            pool([first])


if __name__ == "__main__":
    unittest.main()
