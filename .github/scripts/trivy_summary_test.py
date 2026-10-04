"""Tests for trivy_summary.py (Trivy JSON reports -> step-summary Markdown).

Run: python3 .github/scripts/trivy_summary_test.py
"""
import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "trivy_summary.py")


def report(name, results):
    return {"SchemaVersion": 2, "ArtifactName": name, "Results": results}


CLEAN = report("Cargo.lock", [{"Target": "Cargo.lock", "Type": "cargo"}])
DIRTY = report(
    "gsp-deploy/gsp:local",
    [
        {
            "Target": "Cargo.lock",
            "Type": "cargo",
            "Vulnerabilities": [
                {
                    "VulnerabilityID": "CVE-2026-41676",
                    "PkgName": "openssl",
                    "InstalledVersion": "0.10.38",
                    "FixedVersion": "0.10.78",
                    "Severity": "HIGH",
                    "Title": "a | pipe <and> tags",
                    "PrimaryURL": "https://avd.aquasec.com/nvd/cve-2026-41676",
                },
                {
                    "VulnerabilityID": "CVE-2026-1",
                    "PkgName": "libc6",
                    "InstalledVersion": "2.41-1",
                    "FixedVersion": "2.41-2",
                    "Severity": "CRITICAL",
                },
            ],
        }
    ],
)
SECRET = report(
    "gsp-deploy/gsp-ui:local",
    [
        {
            "Target": "/web/key.pem",
            "Class": "secret",
            "Secrets": [{"RuleID": "private-key", "Severity": "HIGH", "Title": "Asymmetric Private Key"}],
        }
    ],
)


def run(reports, *args, not_scanned=None):
    with tempfile.TemporaryDirectory() as d:
        if not_scanned:
            with open(os.path.join(d, "not-scanned.txt"), "w") as f:
                f.write("\n".join(not_scanned) + "\n")
        for i, r in enumerate(reports):
            with open(os.path.join(d, f"{i}.json"), "w") as f:
                f.write(r if isinstance(r, str) else json.dumps(r))
        out = subprocess.run(
            [sys.executable, SCRIPT, *args, d], capture_output=True, text=True
        )
    return out.returncode, out.stdout


class Summary(unittest.TestCase):
    def test_all_clean(self):
        rc, out = run([CLEAN])
        self.assertEqual(rc, 0)
        self.assertIn("No HIGH or CRITICAL", out)

    def test_findings_are_tabled_and_escaped(self):
        rc, out = run([CLEAN, DIRTY])
        self.assertEqual(rc, 0)
        self.assertIn("2 finding", out)
        self.assertIn("[CVE-2026-41676](https://avd.aquasec.com/nvd/cve-2026-41676)", out)
        self.assertIn("0.10.78", out)
        self.assertIn("a \\| pipe &lt;and&gt; tags", out)
        # Most severe first.
        self.assertLess(out.index("CVE-2026-1"), out.index("CVE-2026-41676"))

    def test_secrets_are_reported(self):
        rc, out = run([SECRET])
        self.assertEqual(rc, 0)
        self.assertIn("private-key", out)
        self.assertIn("/web/key.pem", out)

    def test_a_broken_report_is_a_note_not_a_failure(self):
        rc, out = run(["{not json", CLEAN])
        self.assertEqual(rc, 0)
        self.assertIn("could not read", out)

    def test_annotations_one_per_finding_target(self):
        rc, out = run([CLEAN, DIRTY, SECRET], "--annotations")
        self.assertEqual(rc, 0)
        lines = [l for l in out.splitlines() if l.startswith("::warning")]
        self.assertEqual(len(lines), 2, out)
        self.assertTrue(any("gsp-deploy/gsp:local" in l and "2 " in l for l in lines), out)

    def test_a_target_that_failed_to_scan_is_listed(self):
        rc, out = run([CLEAN], not_scanned=["image-gsp-ui"])
        self.assertEqual(rc, 0)
        self.assertIn("Not scanned", out)
        self.assertIn("image-gsp-ui", out)
        rc, out = run([CLEAN], "--annotations", not_scanned=["image-gsp-ui"])
        self.assertIn("::warning", out)
        self.assertIn("image-gsp-ui", out)

    def test_no_annotations_when_clean(self):
        rc, out = run([CLEAN], "--annotations")
        self.assertEqual((rc, out.strip()), (0, ""))


if __name__ == "__main__":
    unittest.main()
