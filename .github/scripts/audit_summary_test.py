"""Tests for audit_summary.py (cargo audit JSON -> step-summary Markdown).

Run: python3 .github/scripts/audit_summary_test.py
"""
import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "audit_summary.py")


def report(vulns=(), unmaintained=(), yanked=()):
    return {
        "lockfile": {"dependency-count": 42},
        "vulnerabilities": {"found": bool(vulns), "count": len(vulns), "list": list(vulns)},
        "warnings": {"unmaintained": list(unmaintained), "yanked": list(yanked)},
    }


VULN = {
    "advisory": {
        "id": "RUSTSEC-2026-0285",
        "package": "rustls",
        "title": "a | pipe <and> tags",
        "url": "https://github.com/rustls/rustls/security/advisories/x",
    },
    "versions": {"patched": [">=0.23.45"], "unaffected": []},
    "package": {"name": "rustls", "version": "0.23.44"},
}
UNMAINTAINED = {
    "kind": "unmaintained",
    "advisory": {"id": "RUSTSEC-2026-0001", "package": "old", "title": "old is unmaintained"},
    "package": {"name": "old", "version": "1.0.0"},
}
YANKED = {"kind": "yanked", "advisory": None, "package": {"name": "gone", "version": "0.1.0"}}


def run(reports, *args, not_audited=None):
    with tempfile.TemporaryDirectory() as d:
        if not_audited:
            with open(os.path.join(d, "not-audited.txt"), "w") as f:
                f.write("\n".join(not_audited) + "\n")
        for name, r in reports.items():
            with open(os.path.join(d, f"{name}.json"), "w") as f:
                f.write(r if isinstance(r, str) else json.dumps(r))
        out = subprocess.run([sys.executable, SCRIPT, *args, d], capture_output=True, text=True)
    return out.returncode, out.stdout


class Summary(unittest.TestCase):
    def test_all_clean(self):
        rc, out = run({"root": report()})
        self.assertEqual(rc, 0)
        self.assertIn("No vulnerabilities", out)
        self.assertIn("`root`", out)

    def test_vulnerabilities_are_tabled_and_escaped(self):
        rc, out = run({"root": report([VULN]), "crates_plugins": report()})
        self.assertEqual(rc, 0)
        self.assertIn("1 vulnerabilit", out)
        self.assertIn("[RUSTSEC-2026-0285](https://github.com/rustls/rustls/security/advisories/x)", out)
        self.assertIn("0.23.44", out)
        self.assertIn("&gt;=0.23.45", out)
        self.assertIn("a \\| pipe &lt;and&gt; tags", out)

    def test_warnings_are_listed_separately(self):
        rc, out = run({"root": report(unmaintained=[UNMAINTAINED], yanked=[YANKED])})
        self.assertEqual(rc, 0)
        self.assertIn("No vulnerabilities", out)
        self.assertIn("unmaintained", out)
        self.assertIn("RUSTSEC-2026-0001", out)
        self.assertIn("yanked", out)
        self.assertIn("gone", out)

    def test_broken_or_missing_reports_are_notes(self):
        rc, out = run({"root": "{not json"}, not_audited=["crates/plugins/Cargo.lock"])
        self.assertEqual(rc, 0)
        self.assertIn("could not read", out)
        self.assertIn("crates/plugins/Cargo.lock", out)

    def test_annotations(self):
        rc, out = run({"root": report([VULN], unmaintained=[UNMAINTAINED]), "x": report()},
                      "--annotations", not_audited=["crates/plugins/Cargo.lock"])
        self.assertEqual(rc, 0)
        lines = [l for l in out.splitlines() if l.startswith("::warning")]
        self.assertEqual(len(lines), 2, out)
        self.assertTrue(any("root" in l and "1 vulnerab" in l for l in lines), out)

    def test_no_annotations_when_clean(self):
        rc, out = run({"root": report(unmaintained=[UNMAINTAINED])}, "--annotations")
        self.assertEqual((rc, out.strip()), (0, ""))


if __name__ == "__main__":
    unittest.main()
