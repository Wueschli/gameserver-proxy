"""Tests for check_version_policy.py.

Run: python3 .github/scripts/check_version_policy_test.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from check_version_policy import check  # noqa: E402


def toml(version, members=("crates/wayhouse", "crates/wayhouse-core")):
    m = ", ".join(f'"{x}"' for x in members)
    return f'[workspace]\nmembers = [{m}]\n\n[workspace.package]\nversion = "{version}"\n'


def lock(*pkgs):
    out = "version = 4\n"
    for name, version, source in pkgs:
        out += f'\n[[package]]\nname = "{name}"\nversion = "{version}"\n'
        if source:
            out += f'source = "{source}"\n'
    return out


REG = "registry+https://github.com/rust-lang/crates.io-index"


class CheckTest(unittest.TestCase):
    def ok_lock(self, v):
        return lock(("wayhouse", v, None), ("wayhouse-core", v, None))

    def test_plain_0x_version_passes(self):
        self.assertEqual(check(toml("0.3.2"), self.ok_lock("0.3.2")), "0.3.2")

    def test_prerelease_passes(self):
        self.assertEqual(check(toml("0.1.0-rc.1"), self.ok_lock("0.1.0-rc.1")), "0.1.0-rc.1")

    def test_major_1_fails_with_policy_message(self):
        with self.assertRaises(ValueError) as e:
            check(toml("1.0.0"), self.ok_lock("1.0.0"))
        self.assertIn("0.x", str(e.exception))
        self.assertIn("1.0.0", str(e.exception))

    def test_major_1_prerelease_fails(self):
        with self.assertRaises(ValueError):
            check(toml("1.0.0-rc.1"), self.ok_lock("1.0.0-rc.1"))

    def test_major_1_passes_with_allow_major(self):
        self.assertEqual(check(toml("1.0.0"), self.ok_lock("1.0.0"), allow_major=True), "1.0.0")

    def test_malformed_version_fails(self):
        with self.assertRaises(ValueError):
            check(toml("banana"), self.ok_lock("banana"))

    def test_lock_mismatch_fails_and_names_cargo_update(self):
        bad = lock(("wayhouse", "0.1.0", None), ("wayhouse-core", "0.0.1", None))
        with self.assertRaises(ValueError) as e:
            check(toml("0.1.0"), bad)
        msg = str(e.exception)
        self.assertIn("wayhouse-core", msg)
        self.assertIn("cargo update --workspace", msg)

    def test_member_missing_from_lock_fails(self):
        with self.assertRaises(ValueError):
            check(toml("0.1.0"), lock(("wayhouse", "0.1.0", None)))

    def test_lock_entries_for_non_workspace_crates_ignored(self):
        l = self.ok_lock("0.1.0") + lock(("serde", "1.0.0", REG), ("wayhouse", "9.9.9", REG))[len("version = 4\n"):]
        self.assertEqual(check(toml("0.1.0"), l), "0.1.0")


if __name__ == "__main__":
    unittest.main()
