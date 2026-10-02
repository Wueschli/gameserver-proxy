#!/usr/bin/env python3
"""Render Trivy JSON reports (deploy/scan-images.sh) for the CI run page.

    trivy_summary.py <report-dir> >> "$GITHUB_STEP_SUMMARY"   # Markdown table
    trivy_summary.py --annotations <report-dir>                # ::warning lines

The scan is informational: reporting never fails a job. A missing or malformed
report becomes a note, and the exit code is always 0. Tested by
trivy_summary_test.py.
"""
import argparse
import glob
import html
import json
import os

ORDER = {"CRITICAL": 0, "HIGH": 1, "MEDIUM": 2, "LOW": 3, "UNKNOWN": 4}
MAX_ROWS = 100


def esc(text):
    """Escape for a Markdown table cell."""
    return html.escape(str(text or ""), quote=False).replace("|", "\\|").replace("\n", " ")


def findings(report):
    """(artifact, target, severity, id, package, installed, fixed, title, url) rows."""
    artifact = report.get("ArtifactName", "?")
    for result in report.get("Results") or []:
        target = result.get("Target", "?")
        for v in result.get("Vulnerabilities") or []:
            yield (artifact, target, v.get("Severity", "UNKNOWN"), v.get("VulnerabilityID", "?"),
                   v.get("PkgName", ""), v.get("InstalledVersion", ""), v.get("FixedVersion", ""),
                   v.get("Title", ""), v.get("PrimaryURL", ""))
        for s in result.get("Secrets") or []:
            yield (artifact, target, s.get("Severity", "UNKNOWN"), s.get("RuleID", "secret"),
                   "(secret)", "", "", s.get("Title", ""), "")


def load(directory):
    reports, notes = [], []
    for path in sorted(glob.glob(os.path.join(directory, "*.json"))):
        try:
            with open(path) as f:
                reports.append(json.load(f))
        except (OSError, ValueError) as e:
            notes.append(f"could not read `{os.path.basename(path)}`: {esc(e)}")
    missing = []
    try:
        with open(os.path.join(directory, "not-scanned.txt")) as f:
            missing = [line.strip() for line in f if line.strip()]
    except OSError:
        pass
    if not reports and not notes and not missing:
        notes.append(f"no reports in `{esc(directory)}` (did the scan run?)")
    return reports, notes, missing


def markdown(reports, notes, missing=()):
    rows = sorted((r for rep in reports for r in findings(rep)), key=lambda r: (ORDER.get(r[2], 9), r[0], r[3]))
    out = ["### Trivy (informational)", ""]
    scanned = ", ".join(f"`{esc(r.get('ArtifactName', '?'))}`" for r in reports)
    if scanned:
        out += [f"Scanned: {scanned}.", ""]
    if not rows:
        out.append("No HIGH or CRITICAL vulnerabilities with a fix, and no secrets.")
    else:
        out += [f"**{len(rows)} finding(s)** (HIGH/CRITICAL with a fix available, or a secret):", "",
                "| Severity | ID | Package | Installed | Fixed in | Where | Title |",
                "|---|---|---|---|---|---|---|"]
        for art, target, sev, vid, pkg, inst, fixed, title, url in rows[:MAX_ROWS]:
            ident = f"[{esc(vid)}]({url})" if url else esc(vid)
            where = esc(art) if art == target else f"{esc(art)}: {esc(target)}"
            out.append(f"| {esc(sev)} | {ident} | {esc(pkg)} | {esc(inst)} | {esc(fixed)} | {where} | {esc(title)} |")
        if len(rows) > MAX_ROWS:
            out.append(f"\n…and {len(rows) - MAX_ROWS} more (see the `trivy-reports` artifact).")
    if missing:
        out.append("\n**Not scanned** (Trivy failed before writing a report; see the step log): "
                   + ", ".join(f"`{esc(m)}`" for m in missing))
    out += [f"\n> {n}" for n in notes]
    return "\n".join(out) + "\n"


def annotations(reports, missing=()):
    lines = [f"::warning title=Trivy: {m} not scanned::Trivy failed on {m}; see the step log"
             for m in missing]
    for rep in reports:
        n = sum(1 for _ in findings(rep))
        if n:
            art = rep.get("ArtifactName", "?")
            lines.append(f"::warning title=Trivy: {art}::{n} finding(s) in {art}; see the run summary")
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
        print(f"> trivy_summary.py failed: {esc(e)}")
