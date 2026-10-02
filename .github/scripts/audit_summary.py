#!/usr/bin/env python3
"""Render cargo audit JSON reports (cargo_audit.sh) for the CI run page.

    audit_summary.py <report-dir> >> "$GITHUB_STEP_SUMMARY"   # Markdown
    audit_summary.py --annotations <report-dir>                # ::warning lines

Informational: reporting never fails a job. A missing or malformed report
becomes a note, and the exit code is always 0. Vulnerabilities get a table and
an annotation; warnings (unmaintained, unsound, yanked) only a list. Tested by
audit_summary_test.py.
"""
import argparse
import glob
import html
import json
import os

MAX_ROWS = 100


def esc(text):
    """Escape for a Markdown table cell."""
    return html.escape(str(text or ""), quote=False).replace("|", "\\|").replace("\n", " ")


def load(directory):
    reports, notes, missing = {}, [], []
    for path in sorted(glob.glob(os.path.join(directory, "*.json"))):
        name = os.path.splitext(os.path.basename(path))[0]
        try:
            with open(path) as f:
                reports[name] = json.load(f)
        except (OSError, ValueError) as e:
            notes.append(f"could not read `{esc(os.path.basename(path))}`: {esc(e)}")
    try:
        with open(os.path.join(directory, "not-audited.txt")) as f:
            missing = [line.strip() for line in f if line.strip()]
    except OSError:
        pass
    if not reports and not notes and not missing:
        notes.append(f"no reports in `{esc(directory)}` (did cargo audit run?)")
    return reports, notes, missing


def vulns(report):
    return (report.get("vulnerabilities") or {}).get("list") or []


def warnings(report):
    for kind, items in sorted((report.get("warnings") or {}).items()):
        for w in items or []:
            yield kind, w


def markdown(reports, notes, missing):
    out = ["### cargo audit (informational)", ""]
    if reports:
        out += ["Audited: " + ", ".join(f"`{esc(n)}`" for n in reports) + ".", ""]
    rows = [(name, v) for name, rep in reports.items() for v in vulns(rep)]
    if not rows:
        out.append("No vulnerabilities in the RustSec advisory database.")
    else:
        out += [f"**{len(rows)} vulnerabilit{'y' if len(rows) == 1 else 'ies'}:**", "",
                "| ID | Crate | Version | Patched | Lockfile | Title |", "|---|---|---|---|---|---|"]
        for name, v in rows[:MAX_ROWS]:
            adv, pkg = v.get("advisory") or {}, v.get("package") or {}
            vid, url = adv.get("id", "?"), adv.get("url")
            ident = f"[{esc(vid)}]({url})" if url else esc(vid)
            patched = ", ".join((v.get("versions") or {}).get("patched") or []) or "none"
            out.append(f"| {ident} | {esc(pkg.get('name'))} | {esc(pkg.get('version'))} | "
                       f"{esc(patched)} | {esc(name)} | {esc(adv.get('title'))} |")
        if len(rows) > MAX_ROWS:
            out.append(f"\n…and {len(rows) - MAX_ROWS} more (see the `cargo-audit` artifact).")
    warns = [(name, kind, w) for name, rep in reports.items() for kind, w in warnings(rep)]
    if warns:
        out += ["", f"Warnings ({len(warns)}):", ""]
        for name, kind, w in warns[:MAX_ROWS]:
            adv, pkg = w.get("advisory") or {}, w.get("package") or {}
            what = f" — {esc(adv.get('id'))}: {esc(adv.get('title'))}" if adv else ""
            out.append(f"- {esc(kind)}: `{esc(pkg.get('name'))} {esc(pkg.get('version'))}` "
                       f"in `{esc(name)}`{what}")
    if missing:
        out.append("\n**Not audited** (cargo audit failed; see the step log): "
                   + ", ".join(f"`{esc(m)}`" for m in missing))
    out += [f"\n> {n}" for n in notes]
    return "\n".join(out) + "\n"


def annotations(reports, missing):
    lines = [f"::warning title=cargo audit: {m}::cargo audit failed on {m}; see the step log"
             for m in missing]
    for name, rep in reports.items():
        n = len(vulns(rep))
        if n:
            lines.append(f"::warning title=cargo audit: {name}::{n} vulnerabilit"
                         f"{'y' if n == 1 else 'ies'} in {name}; see the run summary")
    return lines


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--annotations", action="store_true")
    ap.add_argument("dir")
    args = ap.parse_args()
    reports, notes, missing = load(args.dir)
    if args.annotations:
        for line in annotations(reports, missing):
            print(line)
    else:
        print(markdown(reports, notes, missing))


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # never fail the job over a report
        print(f"> audit_summary.py failed: {esc(e)}")
