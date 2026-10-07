#!/usr/bin/env python3
"""Tests for update_sniffers_lock.py (run: python3 .github/scripts/update_sniffers_lock_test.py)."""
import json
import pathlib
import subprocess
import sys
import tempfile
import tomllib
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from fetch_sniffers import load_lock  # noqa: E402

SCRIPT = pathlib.Path(__file__).with_name("update_sniffers_lock.py")
BASE = "https://github.com/wayhouse-proxy/sniffers/releases/download"
H = lambda c: c * 64  # noqa: E731


def entry(name, versions):
    return {"name": name, "versions": [
        {"version": v, "abi": "0.1", "url": f"{BASE}/{name}-v{v}/{name}.wasm", "sha256": h}
        for v, h in versions
    ]}


class UpdateLock(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def run_it(self, index, *extra):
        idx = self.root / "index.json"
        idx.write_text(json.dumps(index))
        out = self.root / "sniffers.lock"
        r = subprocess.run(
            [sys.executable, str(SCRIPT), "--index", str(idx), "--out", str(out), *extra],
            capture_output=True, text=True,
        )
        return r, out

    def test_pins_the_newest_version_of_each_sniffer(self):
        r, out = self.run_it({"sniffers": [
            entry("a2s", [("0.2.0", H("b")), ("0.1.0", H("a"))]),
            entry("quic", [("0.1.0", H("c"))]),
        ]})
        self.assertEqual(r.returncode, 0, r.stderr)
        lock = tomllib.loads(out.read_text())
        self.assertEqual(lock["repo"], "wayhouse-proxy/sniffers")
        self.assertEqual(lock["sniffers"]["a2s"], {"tag": "a2s-v0.2.0", "file": "a2s.wasm", "sha256": H("b")})
        self.assertEqual(lock["sniffers"]["quic"]["tag"], "quic-v0.1.0")

    def test_only_filters_to_the_named_sniffers(self):
        r, out = self.run_it({"sniffers": [entry("a2s", [("0.1.0", H("a"))]), entry("quic", [("0.1.0", H("c"))])]},
                             "--only", "a2s")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(list(tomllib.loads(out.read_text())["sniffers"]), ["a2s"])

    def test_refuses_a_url_that_is_not_a_release_download(self):
        bad = entry("a2s", [("0.1.0", H("a"))])
        bad["versions"][0]["url"] = "https://example.com/a2s.wasm"
        r, out = self.run_it({"sniffers": [bad]})
        self.assertNotEqual(r.returncode, 0)
        self.assertFalse(out.exists())

    def test_output_is_deterministic_and_loads_in_the_fetcher(self):
        index = {"sniffers": [entry("quic", [("0.1.0", H("c"))]), entry("a2s", [("0.1.0", H("a"))])]}
        _, out = self.run_it(index)
        first = out.read_text()
        load_lock(out)
        _, out = self.run_it(index)
        self.assertEqual(first, out.read_text())
        self.assertLess(first.index("[sniffers.a2s]"), first.index("[sniffers.quic]"))


if __name__ == "__main__":
    unittest.main()
