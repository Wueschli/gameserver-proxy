"""Tests for test_summary.py (JUnit XML -> GitHub step-summary Markdown).

Run: python3 .github/scripts/test_summary_test.py
"""
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "test_summary.py")

NEXTEST_PASS = """<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="2" failures="0" errors="0" uuid="u" timestamp="t" time="0.5">
  <testsuite name="gsp-core" tests="2" disabled="0" errors="0" failures="0">
    <testcase name="pool::tests::a" classname="gsp-core" timestamp="t" time="0.010"/>
    <testcase name="pool::tests::b" classname="gsp-core" timestamp="t" time="0.250"/>
  </testsuite>
</testsuites>"""

NEXTEST_FAIL = """<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="3" failures="1" errors="0" time="1.0">
  <testsuite name="gsp-config" tests="3" disabled="0" errors="0" failures="1">
    <testcase name="ok_one" classname="gsp-config" time="0.1"/>
    <testcase name="skipped_one" classname="gsp-config" time="0.0"><skipped/></testcase>
    <testcase name="broken" classname="gsp-config" time="0.2">
      <failure message="assertion failed: left == right" type="test failure">thread 'broken' panicked at src/lib.rs:9
left: 1
right: 2</failure>
      <system-out>captured stdout line</system-out>
    </testcase>
  </testsuite>
</testsuites>"""

VITEST = """<?xml version="1.0" encoding="UTF-8" ?>
<testsuites name="vitest tests" tests="2" failures="0" errors="0" time="1.2">
    <testsuite name="src/App.test.tsx" timestamp="t" hostname="h" tests="2" failures="0" errors="0" skipped="0" time="0.9">
        <testcase classname="src/App.test.tsx" name="renders the fleet" time="0.4"></testcase>
        <testcase classname="src/App.test.tsx" name="confirms drain" time="0.5"></testcase>
    </testsuite>
</testsuites>"""

SPECIAL = """<testsuites><testsuite name="s" tests="1" failures="0">
<testcase name="parse::&lt;Vec&lt;u8&gt;&gt; | pipe" classname="s" time="0.1"/></testsuite></testsuites>"""


def run(title, *contents, missing=()):
    """Write each content to a temp file, run the script, return (rc, stdout)."""
    with tempfile.TemporaryDirectory() as d:
        paths = []
        for i, c in enumerate(contents):
            p = os.path.join(d, f"r{i}.xml")
            with open(p, "w") as f:
                f.write(c)
            paths.append(p)
        paths += [os.path.join(d, m) for m in missing]
        r = subprocess.run(
            [sys.executable, SCRIPT, "--title", title, *paths],
            capture_output=True, text=True,
        )
        return r.returncode, r.stdout


class SummaryTests(unittest.TestCase):
    def test_all_pass_headline_and_suite_table(self):
        rc, out = run("test", NEXTEST_PASS)
        self.assertEqual(rc, 0)
        self.assertIn("### test", out)
        self.assertIn("✅ 2 passed · 0 failed · 0 skipped", out)
        self.assertIn("| gsp-core | 2 | 0 | 0 |", out)
        self.assertNotIn("Failures", out)

    def test_failure_is_listed_with_message_and_output(self):
        rc, out = run("test", NEXTEST_FAIL)
        self.assertEqual(rc, 0)  # reporting never fails the job
        self.assertIn("❌ 1 passed · 1 failed · 1 skipped", out)
        self.assertIn("Failures", out)
        self.assertIn("gsp-config::broken", out)
        self.assertIn("assertion failed: left == right", out)
        self.assertIn("panicked at src/lib.rs:9", out)
        self.assertIn("captured stdout line", out)

    def test_long_failure_output_is_truncated(self):
        big = NEXTEST_FAIL.replace("right: 2", "x" * 20000)
        _, out = run("test", big)
        self.assertIn("truncated", out)
        self.assertLess(len(out), 12000)

    def test_headline_time_is_wall_clock_from_the_report_root(self):
        # Tests run in parallel: summing per-test times (0.26s here) overstates
        # the run; the <testsuites time="0.5"> root is the wall clock.
        _, out = run("test", NEXTEST_PASS)
        self.assertIn("· 0.50s", out.splitlines()[2])
        _, out = run("mixed", NEXTEST_PASS, VITEST)   # 0.5 + 1.2
        self.assertIn("· 1.70s", out.splitlines()[2])

    def test_slowest_tests_sorted_desc(self):
        _, out = run("test", NEXTEST_PASS)
        self.assertIn("Slowest", out)
        self.assertLess(out.index("pool::tests::b"), out.index("pool::tests::a"))

    def test_multiple_files_are_aggregated(self):
        _, out = run("mixed", NEXTEST_PASS, VITEST)
        self.assertIn("✅ 4 passed · 0 failed · 0 skipped", out)
        self.assertIn("| src/App.test.tsx | 2 | 0 | 0 |", out)
        self.assertIn("| gsp-core | 2 | 0 | 0 |", out)

    def test_missing_report_is_a_note_not_a_crash(self):
        rc, out = run("tunnel", missing=("nope.xml",))
        self.assertEqual(rc, 0)
        self.assertIn("no test report", out.lower())

    def test_malformed_xml_is_a_note_not_a_crash(self):
        rc, out = run("test", "<testsuites><oops")
        self.assertEqual(rc, 0)
        self.assertIn("could not parse", out.lower())

    def test_markdown_special_characters_are_escaped_in_tables(self):
        _, out = run("test", SPECIAL)
        self.assertIn("&lt;Vec&lt;u8&gt;&gt; \\| pipe", out)
        self.assertNotIn("<Vec", out)


if __name__ == "__main__":
    unittest.main(verbosity=2)
