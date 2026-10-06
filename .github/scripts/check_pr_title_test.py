"""Tests for check_pr_title.py.

Run: python3 .github/scripts/check_pr_title_test.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from check_pr_title import valid  # noqa: E402


class TitleTest(unittest.TestCase):
    def test_accepts_conventional_titles(self):
        for t in ["feat: x", "fix(controller): y", "feat!: breaking",
                  "chore(main): release 0.1.0", "feat(ui)!: drop it"]:
            self.assertTrue(valid(t), t)

    def test_rejects_other_titles(self):
        for t in ["Add thing", "Feat: x", "fix:x", "", "feat: ", "wip: x", "fix(): y"]:
            self.assertFalse(valid(t), t)

    def test_length_limit_is_100(self):
        self.assertTrue(valid("feat: " + "a" * 94))
        self.assertFalse(valid("feat: " + "a" * 95))

    def test_multiline_title_rejected(self):
        self.assertFalse(valid("feat: x\nfix: y"))


if __name__ == "__main__":
    unittest.main()
