#!/usr/bin/env python3
"""Render JUnit XML (cargo-nextest, vitest) as Markdown for $GITHUB_STEP_SUMMARY.

    test_summary.py --title "test" report1.xml [report2.xml ...] >> "$GITHUB_STEP_SUMMARY"

Reporting must never fail a job: a missing or malformed report becomes a note,
and the exit code is always 0. Tested by test_summary_test.py.
"""
import argparse
import html
import os
import sys
import xml.etree.ElementTree as ET

MAX_FAILURES = 20          # failures rendered in full
MAX_OUTPUT_CHARS = 3000    # per failure
SLOWEST = 5


def esc_cell(text):
    """Escape for a Markdown table cell (names contain <Generics> and |)."""
    return html.escape(text, quote=False).replace("|", "\\|")


def fmt_time(seconds):
    if seconds >= 60:
        return f"{int(seconds // 60)}m{seconds % 60:04.1f}s"
    return f"{seconds:.2f}s" if seconds < 10 else f"{seconds:.1f}s"


class Case:
    def __init__(self, suite, el):
        self.suite = suite
        classname, name = el.get("classname") or suite, el.get("name") or "?"
        self.name = f"{classname}::{name}"
        self.time = float(el.get("time") or 0)
        bad = el.find("failure")
        if bad is None:
            bad = el.find("error")
        self.skipped = el.find("skipped") is not None
        self.failed = bad is not None
        self.message = (bad.get("message") or "") if bad is not None else ""
        parts = []
        if bad is not None and bad.text and bad.text.strip():
            parts.append(bad.text.strip())
        for tag in ("system-out", "system-err"):
            for s in el.findall(tag):
                if s.text and s.text.strip():
                    parts.append(f"[{tag}]\n{s.text.strip()}")
        self.output = "\n".join(parts)


def load(path):
    """-> (cases, wall_seconds_or_None, note). Never raises."""
    if not os.path.exists(path):
        return [], None, f"No test report found at `{path}` (the tests did not get as far as writing one)."
    try:
        root = ET.parse(path).getroot()
    except (ET.ParseError, OSError) as e:
        return [], None, f"Could not parse `{path}`: {e}"
    try:
        wall = float(root.get("time")) if root.get("time") else None
    except ValueError:
        wall = None
    cases = []
    for suite in root.iter("testsuite"):
        sname = suite.get("name") or "?"
        for el in suite.findall("testcase"):
            cases.append(Case(sname, el))
    return cases, wall, None


def render(title, paths):
    cases, notes, walls = [], [], []
    for p in paths:
        c, wall, note = load(p)
        cases += c
        walls.append(wall if wall is not None else sum(x.time for x in c))
        if note:
            notes.append(note)

    out = [f"### {title}", ""]
    passed = sum(1 for c in cases if not c.failed and not c.skipped)
    failed = sum(1 for c in cases if c.failed)
    skipped = sum(1 for c in cases if c.skipped)
    # Tests run in parallel, so summing per-test times overstates the run:
    # use the reports' own wall-clock totals.
    total_time = sum(walls)
    if cases:
        icon = "❌" if failed else "✅"
        out.append(f"{icon} {passed} passed · {failed} failed · {skipped} skipped · {fmt_time(total_time)}")
    elif not notes:
        out.append("⚠️ 0 tests ran")
    for n in notes:
        out += ["", f"> ⚠️ {n}"]
    if not cases:
        return "\n".join(out) + "\n"

    # per-suite table, in first-seen order
    suites = {}
    for c in cases:
        s = suites.setdefault(c.suite, [0, 0, 0, 0.0])
        s[0] += 1
        s[1] += c.failed
        s[2] += c.skipped
        s[3] += c.time
    out += ["", "| suite | tests | failed | skipped | test time |", "|---|---:|---:|---:|---:|"]
    for name, (n, f, sk, t) in suites.items():
        out.append(f"| {esc_cell(name)} | {n} | {f} | {sk} | {fmt_time(t)} |")

    slow = sorted((c for c in cases if not c.skipped), key=lambda c: c.time, reverse=True)[:SLOWEST]
    out += ["", f"**Slowest {len(slow)}**", "", "| test | time |", "|---|---:|"]
    for c in slow:
        out.append(f"| {esc_cell(c.name)} | {fmt_time(c.time)} |")

    bad = [c for c in cases if c.failed]
    if bad:
        out += ["", f"#### Failures ({len(bad)})", ""]
        for c in bad[:MAX_FAILURES]:
            body = (c.message + ("\n" + c.output if c.output else "")).strip()
            if len(body) > MAX_OUTPUT_CHARS:
                body = body[:MAX_OUTPUT_CHARS] + f"\n… (output truncated at {MAX_OUTPUT_CHARS} chars)"
            body = body.replace("```", "'''")
            out += [
                f"<details><summary>❌ <code>{html.escape(c.name, quote=False)}</code></summary>",
                "",
                "```",
                body,
                "```",
                "</details>",
                "",
            ]
        if len(bad) > MAX_FAILURES:
            out.append(f"… and {len(bad) - MAX_FAILURES} more failures (see the uploaded JUnit XML).")
    return "\n".join(out) + "\n"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--title", required=True)
    ap.add_argument("paths", nargs="*")
    args = ap.parse_args()
    sys.stdout.write(render(args.title, args.paths))
    return 0


if __name__ == "__main__":
    sys.exit(main())
