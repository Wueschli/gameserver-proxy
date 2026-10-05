"""Tests for check_release.py.

Run: python3 .github/scripts/check_release_test.py
"""
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from check_release import check  # noqa: E402


class CheckTest(unittest.TestCase):
    def cargo(self, body):
        d = tempfile.TemporaryDirectory()
        self.addCleanup(d.cleanup)
        path = os.path.join(d.name, "Cargo.toml")
        with open(path, "w") as f:
            f.write(body)
        return path

    def test_matching_tag_returns_the_version(self):
        p = self.cargo('[workspace.package]\nversion = "0.1.0"\n')
        self.assertEqual(check("v0.1.0", p), "0.1.0")

    def test_prerelease_must_match_exactly(self):
        p = self.cargo('[workspace.package]\nversion = "0.1.0-rc.1"\n')
        self.assertEqual(check("v0.1.0-rc.1", p), "0.1.0-rc.1")
        with self.assertRaises(ValueError):
            check("v0.1.0", p)

    def test_mismatch_names_both_versions(self):
        p = self.cargo('[workspace.package]\nversion = "0.0.1"\n')
        with self.assertRaises(ValueError) as e:
            check("v0.1.0", p)
        self.assertIn("0.0.1", str(e.exception))
        self.assertIn("v0.1.0", str(e.exception))

    def test_rejects_a_malformed_tag(self):
        p = self.cargo('[workspace.package]\nversion = "0.1.0"\n')
        for tag in ("0.1.0", "v0.1", "v0.1.0.1", "v0.1.0 ", "latest"):
            with self.assertRaises(ValueError, msg=tag):
                check(tag, p)

    def test_missing_workspace_version(self):
        p = self.cargo("[workspace]\nmembers = []\n")
        with self.assertRaises(ValueError):
            check("v0.1.0", p)

    def test_the_real_workspace_version_passes_its_own_tag(self):
        root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "Cargo.toml")
        with open(root, "rb") as f:
            import tomllib
            v = tomllib.load(f)["workspace"]["package"]["version"]
        self.assertEqual(check(f"v{v}", root), v)


if __name__ == "__main__":
    unittest.main()
