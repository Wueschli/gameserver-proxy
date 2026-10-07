#!/usr/bin/env python3
"""Gate: the official sniffers live in wayhouse-proxy/sniffers, not here.

Fails when `crates/sniffers` exists again, or when a tracked file still points at
`crates/sniffers` or says `make sniffers` (the removed target; `make sniffers-fetch` is fine).
`docs/superpowers/` (dated plans and specs) and CHANGELOG.md are history and exempt.

Run: python3 .github/scripts/no_bundled_sniffers_test.py   (needs git)
"""
import os
import re
import subprocess
import sys
import unittest

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
STALE = re.compile(r"crates/sniffers\b|make sniffers(?![-\w])")
EXEMPT = ("docs/superpowers/", "CHANGELOG.md", ".github/scripts/no_bundled_sniffers_test.py")


class NoBundledSniffers(unittest.TestCase):
    def test_the_directory_is_gone(self):
        self.assertFalse(os.path.exists(os.path.join(REPO, "crates/sniffers")))

    def test_no_tracked_file_points_at_it(self):
        files = subprocess.run(
            ["git", "-C", REPO, "ls-files", "-z"], check=True, capture_output=True
        ).stdout.decode().split("\0")
        hits = []
        for f in filter(None, files):
            if f.startswith(EXEMPT[0]) or f in EXEMPT[1:] or not os.path.isfile(os.path.join(REPO, f)):
                continue
            try:
                with open(os.path.join(REPO, f), encoding="utf-8") as fh:
                    text = fh.read()
            except (UnicodeDecodeError, OSError):
                continue  # binary
            for n, line in enumerate(text.splitlines(), 1):
                if STALE.search(line):
                    hits.append(f"{f}:{n}: {line.strip()[:100]}")
        self.assertEqual(hits, [], "stale references to the bundled sniffers:\n" + "\n".join(hits))


if __name__ == "__main__":
    sys.dont_write_bytecode = True
    unittest.main()
