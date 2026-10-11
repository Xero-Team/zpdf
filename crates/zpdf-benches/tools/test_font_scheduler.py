import copy
import unittest
from font_pair import measurement_environment
from font_pool import pool
from test_font_pool import report


class SchedulerTests(unittest.TestCase):
    def test_override_preserves_all_host_fields_and_original_evidence(self):
        original = {"cpu": "M5", "rustc": "fixed", "settings": {"scheduler": "background", "sample_size": 50}}
        frozen = copy.deepcopy(original)
        actual = measurement_environment(original, "default")
        self.assertEqual(original, frozen)
        self.assertEqual(actual["settings"]["scheduler"], "default")
        self.assertEqual(actual["cpu"], original["cpu"])
        self.assertEqual(actual["rustc"], original["rustc"])
        self.assertEqual(actual["settings"]["sample_size"], 50)
        self.assertEqual(measurement_environment(original), original)

    def test_different_runtime_schedulers_cannot_be_pooled(self):
        first = report("first", {"first": 100, "second": 100})
        second = report("second", {"first": 100, "second": 100})
        first["environment"]["settings"] = {"scheduler": "background"}
        second["environment"]["settings"] = {"scheduler": "default"}
        with self.assertRaises(ValueError):
            pool([first, second])
        with self.assertRaises(ValueError):
            measurement_environment(first["environment"], "unrecorded")


if __name__ == "__main__":
    unittest.main()
