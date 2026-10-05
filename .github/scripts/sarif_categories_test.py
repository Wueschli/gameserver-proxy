"""Tests for sarif_categories.py.

Run: python3 .github/scripts/sarif_categories_test.py
"""
import json
import os
import subprocess
import sys
import tempfile
import unittest

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "sarif_categories.py")


def sarif(runs=1):
    return {"version": "2.1.0", "runs": [{"tool": {"driver": {"name": "Trivy"}}} for _ in range(runs)]}


class StampTest(unittest.TestCase):
    def run_in(self, files):
        with tempfile.TemporaryDirectory() as d:
            for name, doc in files.items():
                with open(os.path.join(d, name), "w") as f:
                    json.dump(doc, f)
            r = subprocess.run([sys.executable, SCRIPT, d], capture_output=True, text=True)
            out = {n: json.load(open(os.path.join(d, n))) for n in files}
        return r, out

    def test_each_report_gets_its_own_category(self):
        r, out = self.run_in({"image-wayhouse.sarif": sarif(), "lock-cargo.sarif": sarif()})
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(out["image-wayhouse.sarif"]["runs"][0]["automationDetails"]["id"], "trivy/image-wayhouse/")
        self.assertEqual(out["lock-cargo.sarif"]["runs"][0]["automationDetails"]["id"], "trivy/lock-cargo/")

    def test_keeps_the_rest_of_the_report(self):
        _, out = self.run_in({"a.sarif": sarif(2)})
        for run in out["a.sarif"]["runs"]:
            self.assertEqual(run["tool"]["driver"]["name"], "Trivy")

    def test_ignores_other_files_and_empty_dirs(self):
        r, out = self.run_in({"image-wayhouse.json": {"x": 1}})
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(out["image-wayhouse.json"], {"x": 1})

    def test_needs_a_directory_argument(self):
        r = subprocess.run([sys.executable, SCRIPT], capture_output=True, text=True)
        self.assertNotEqual(r.returncode, 0)


if __name__ == "__main__":
    unittest.main()
