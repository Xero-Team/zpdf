import copy
import tempfile
import unittest
from pathlib import Path

from font_pair import recovered_pair, validate_resume, write_json
from test_font_pool import report


class ResumeTests(unittest.TestCase):
    def template(self):
        value = report("archive", {"first": 100, "second": 101})
        value.update(status="in_progress", complete=True, selected_cases=["first", "second"])
        return value

    def test_complete_and_partial_pairs_survive_validation_unchanged(self):
        template = self.template()
        saved = copy.deepcopy(template)
        saved["cases"]["second"]["observations"] = saved["cases"]["second"]["observations"][:2]
        saved["cases"]["second"]["status"] = "in_progress"
        frozen = copy.deepcopy(saved)
        validate_resume(saved, template, template["selected_cases"])
        self.assertEqual(saved, frozen)
        del saved["selected_cases"]
        validate_resume(saved, template, template["selected_cases"])

    def test_changes_to_protocol_executables_inventory_or_timings_fail(self):
        template = self.template()
        for change in [lambda r: r["settings"].update(rounds=6),
                       lambda r: r.update(executables=["a", "changed"]),
                       lambda r: r.update(selected_cases=["first"]),
                       lambda r: r.update(status="pass"),
                       lambda r: r["cases"]["first"]["observations"][0].update(ratio=0.5)]:
            saved = copy.deepcopy(template)
            change(saved)
            with self.assertRaises(ValueError):
                validate_resume(saved, template, template["selected_cases"])

    def test_only_two_verified_sides_form_a_recoverable_pair(self):
        with tempfile.TemporaryDirectory() as root:
            directory = Path(root)
            executables = [Path("before"), Path("after")]
            for side in (0, 1):
                (directory / str(side)).mkdir()
            write_json(directory / "0/completed.json", {"case": "case", "executable": "before", "median_ns": 100, "gpu": None})
            self.assertIsNone(recovered_pair(directory, "case", executables, None))
            write_json(directory / "1/completed.json", {"case": "case", "executable": "after", "median_ns": 101, "gpu": None})
            self.assertEqual(recovered_pair(directory, "case", executables, None), [100, 101])
            with self.assertRaises(ValueError):
                recovered_pair(directory, "different", executables, None)
            self.assertFalse(list(directory.rglob("*.tmp")))


if __name__ == "__main__":
    unittest.main()
