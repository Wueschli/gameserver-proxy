"""Tests for check_md_links.py.

Run: python3 .github/scripts/check_md_links_test.py
"""
import os
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from check_md_links import check_files, slug  # noqa: E402


class LinksTest(unittest.TestCase):
    def tree(self, files):
        d = tempfile.TemporaryDirectory()
        self.addCleanup(d.cleanup)
        for name, body in files.items():
            path = os.path.join(d.name, name)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w") as f:
                f.write(body)
        return d.name

    def run_check(self, files, *targets):
        root = self.tree(files)
        return check_files([os.path.join(root, t) for t in targets])

    def test_existing_relative_link_passes(self):
        self.assertEqual(self.run_check({"a.md": "[b](b.md)\n", "b.md": "# B\n"}, "a.md"), [])

    def test_missing_file_fails(self):
        errs = self.run_check({"a.md": "x\n[b](nope.md)\n"}, "a.md")
        self.assertEqual(len(errs), 1)
        self.assertIn("a.md:2: broken link target nope.md", errs[0])

    def test_missing_anchor_fails_and_present_anchor_passes(self):
        files = {"a.md": "[ok](b.md#hello-world) [bad](b.md#nope) [self](#top)\n# Top\n",
                 "b.md": "## Hello, World!\n"}
        errs = self.run_check(files, "a.md")
        self.assertEqual(len(errs), 1)
        self.assertIn("b.md#nope", errs[0])

    def test_external_and_mailto_links_ignored(self):
        body = "[x](https://example.com/a.md) [m](mailto:a@b.c) [h](http://x/y)\n"
        self.assertEqual(self.run_check({"a.md": body}, "a.md"), [])

    def test_links_inside_code_fences_ignored(self):
        body = "```sh\ngrep -on \"](./nope.md)\" x\n```\n`[a](nope.md)`\n"
        self.assertEqual(self.run_check({"a.md": body}, "a.md"), [])

    def test_directory_links_pass(self):
        self.assertEqual(self.run_check({"a.md": "[d](d/)\n", "d/x.md": ""}, "a.md"), [])

    def test_slug_follows_github(self):
        self.assertEqual(slug("Build and test"), "build-and-test")
        self.assertEqual(slug("Model A: trunk (default)"), "model-a-trunk-default")
        self.assertEqual(slug("`make check`"), "make-check")


if __name__ == "__main__":
    unittest.main(verbosity=1)
