#!/usr/bin/env python3
"""Tests for fetch_sniffers.py (run: python3 .github/scripts/fetch_sniffers_test.py)."""
import functools
import hashlib
import http.server
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
import unittest

SCRIPT = pathlib.Path(__file__).with_name("fetch_sniffers.py")


def sha(b):
    return hashlib.sha256(b).hexdigest()


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *a):
        pass


class FetchTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name)
        self.web = self.root / "web"
        self.web.mkdir()
        handler = functools.partial(Quiet, directory=str(self.web))
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.base = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.out = self.root / "out"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.tmp.cleanup()

    def publish(self, name, data):
        d = self.web / f"{name}-v0.1.0"
        d.mkdir(exist_ok=True)
        (d / f"{name}.wasm").write_bytes(data)

    def lock(self, **sniffers):
        """sniffers: name -> sha256 to pin."""
        text = 'repo = "wayhouse-proxy/sniffers"\n'
        for name, digest in sniffers.items():
            text += f'\n[sniffers.{name}]\ntag = "{name}-v0.1.0"\nfile = "{name}.wasm"\nsha256 = "{digest}"\n'
        p = self.root / "sniffers.lock"
        p.write_text(text)
        return p

    def run_fetch(self, lock, *extra, test_mode=True):
        env = dict(os.environ)
        args = [sys.executable, str(SCRIPT), "--lock", str(lock), "--out", str(self.out), *extra]
        if test_mode:
            env["WAYHOUSE_FETCH_TEST"] = "1"
            args += ["--allow-insecure-test-url", self.base]
        return subprocess.run(args, capture_output=True, text=True, env=env)

    def test_downloads_and_verifies(self):
        self.publish("a2s", b"AAA")
        r = self.run_fetch(self.lock(a2s=sha(b"AAA")))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual((self.out / "a2s.wasm").read_bytes(), b"AAA")
        self.assertIn(str(self.out), r.stdout)

    def test_hash_mismatch_fails_and_names_the_sniffer(self):
        self.publish("a2s", b"AAA")
        r = self.run_fetch(self.lock(a2s=sha(b"other")))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("a2s", r.stderr)
        self.assertIn("sha256", r.stderr)
        self.assertFalse((self.out / "a2s.wasm").exists(), "a bad download must not be kept")

    def test_refuses_http_without_the_test_flag(self):
        self.publish("a2s", b"AAA")
        # The repo in the lock is github.com over https; with no test flag nothing may
        # be fetched over plain http, whatever the environment says.
        env = dict(os.environ, WAYHOUSE_FETCH_TEST="1")
        r = subprocess.run(
            [sys.executable, str(SCRIPT), "--lock", str(self.lock(a2s=sha(b"AAA"))),
             "--out", str(self.out), "--base-url", self.base],
            capture_output=True, text=True, env=env,
        )
        self.assertNotEqual(r.returncode, 0)

    def test_test_flag_needs_the_environment_variable(self):
        self.publish("a2s", b"AAA")
        r = self.run_fetch(self.lock(a2s=sha(b"AAA")), test_mode=False)
        env = {k: v for k, v in os.environ.items() if k != "WAYHOUSE_FETCH_TEST"}
        r = subprocess.run(
            [sys.executable, str(SCRIPT), "--lock", str(self.root / "sniffers.lock"),
             "--out", str(self.out), "--allow-insecure-test-url", self.base],
            capture_output=True, text=True, env=env,
        )
        self.assertNotEqual(r.returncode, 0)
        self.assertFalse((self.out / "a2s.wasm").exists())

    def test_partial_failure_reports_all(self):
        self.publish("a2s", b"AAA")
        self.publish("quic", b"QQQ")
        lock = self.lock(a2s=sha(b"wrong1"), minecraft=sha(b"x"), quic=sha(b"QQQ"))
        r = self.run_fetch(lock)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("a2s", r.stderr)
        self.assertIn("minecraft", r.stderr)  # not published at all
        self.assertTrue((self.out / "quic.wasm").exists())

    def test_second_run_skips_files_with_matching_hash(self):
        self.publish("a2s", b"AAA")
        lock = self.lock(a2s=sha(b"AAA"))
        self.assertEqual(self.run_fetch(lock).returncode, 0)
        # Take the server away: a second run must not need the network.
        (self.web / "a2s-v0.1.0" / "a2s.wasm").unlink()
        r = self.run_fetch(lock)
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_a_stale_file_with_the_wrong_hash_is_replaced(self):
        self.publish("a2s", b"AAA")
        self.out.mkdir()
        (self.out / "a2s.wasm").write_bytes(b"stale")
        r = self.run_fetch(self.lock(a2s=sha(b"AAA")))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual((self.out / "a2s.wasm").read_bytes(), b"AAA")

    def test_rejects_a_lock_with_a_bad_hash_or_path(self):
        bad = self.root / "sniffers.lock"
        bad.write_text('repo = "x/y"\n[sniffers.a2s]\ntag = "t"\nfile = "../evil.wasm"\nsha256 = "' + "0" * 64 + '"\n')
        self.assertNotEqual(self.run_fetch(bad).returncode, 0)
        bad.write_text('repo = "x/y"\n[sniffers.a2s]\ntag = "t"\nfile = "a2s.wasm"\nsha256 = "zz"\n')
        self.assertNotEqual(self.run_fetch(bad).returncode, 0)


if __name__ == "__main__":
    unittest.main()
