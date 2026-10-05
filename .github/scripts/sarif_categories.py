#!/usr/bin/env python3
"""Gives each Trivy SARIF report its own code-scanning category.

github/codeql-action/upload-sarif takes the whole target/trivy directory, but
GitHub rejects an upload whose runs share one tool + category, and every Trivy
report is a "Trivy" run. This stamps `automationDetails.id` with the report's
name (image-wayhouse, lock-cargo, ...), so each report is its own category and a
later upload replaces only that report's alerts.

Usage: sarif_categories.py <report-dir>   (rewrites *.sarif in place)
Tested by sarif_categories_test.py.
"""
import glob
import json
import os
import sys


def stamp(path: str) -> None:
    name = os.path.splitext(os.path.basename(path))[0]
    with open(path) as f:
        doc = json.load(f)
    for run in doc.get("runs", []):
        run["automationDetails"] = {"id": f"trivy/{name}/"}
    with open(path, "w") as f:
        json.dump(doc, f)


def main(argv) -> None:
    if len(argv) != 1:
        sys.exit(__doc__)
    for path in sorted(glob.glob(os.path.join(argv[0], "*.sarif"))):
        stamp(path)


if __name__ == "__main__":
    main(sys.argv[1:])
