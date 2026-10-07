#!/usr/bin/env python3
"""Write sniffers.lock from the sniffers registry index.

    python3 .github/scripts/update_sniffers_lock.py [--index URL|PATH] [--out sniffers.lock] [--only NAME ...]

Pins the newest version of every sniffer (or only the named ones) at the tag and sha256 the
index publishes. The default index is the official one on the sniffers repo's `main`. Review
the diff: the e2e job (`fetch_sniffers.py`) then downloads exactly those bytes. Only
`https://github.com/<owner>/<repo>/releases/download/<tag>/<file>` URLs are accepted.
"""
import argparse
import json
import pathlib
import re
import sys
import urllib.request

from fetch_sniffers import HttpsOnly

OFFICIAL = "https://raw.githubusercontent.com/wayhouse-proxy/sniffers/main/index.json"
URL = re.compile(r"^https://github\.com/([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)/releases/download/([A-Za-z0-9][A-Za-z0-9._-]*)/([A-Za-z0-9][A-Za-z0-9._-]*)$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")


def load_index(src):
    if src.startswith("https://"):
        with urllib.request.build_opener(HttpsOnly).open(src, timeout=60) as r:
            return json.loads(r.read(1 << 20))
    return json.loads(pathlib.Path(src).read_text())


def build_lock(index, only):
    repo, entries = None, {}
    for s in index["sniffers"]:
        name = s["name"]
        if only and name not in only:
            continue
        v = s["versions"][0]  # newest first, per the index format
        m = URL.match(v["url"])
        if not m:
            raise ValueError(f"{name}: not a GitHub release download URL: {v['url']}")
        if not HEX64.match(v["sha256"]):
            raise ValueError(f"{name}: bad sha256")
        if repo not in (None, m[1]):
            raise ValueError(f"{name}: comes from {m[1]}, other sniffers from {repo}")
        repo = m[1]
        entries[name] = (m[2], m[3], v["sha256"])
    missing = set(only or ()) - set(entries)
    if missing:
        raise ValueError(f"not in the index: {', '.join(sorted(missing))}")
    if not entries:
        raise ValueError("the index lists no sniffers")
    lines = [
        "# The official sniffers the `sniffers-e2e` CI job loads, pinned by release tag and sha256.",
        "# Regenerate with: python3 .github/scripts/update_sniffers_lock.py (docs/sniffers.md).",
        f'repo = "{repo}"',
    ]
    for name in sorted(entries):
        tag, file, digest = entries[name]
        lines += ["", f"[sniffers.{name}]", f'tag = "{tag}"', f'file = "{file}"', f'sha256 = "{digest}"']
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--index", default=OFFICIAL)
    ap.add_argument("--out", default="sniffers.lock")
    ap.add_argument("--only", nargs="*")
    a = ap.parse_args()
    try:
        text = build_lock(load_index(a.index), set(a.only or ()))
    except (OSError, ValueError, KeyError, IndexError, json.JSONDecodeError) as e:
        print(f"update_sniffers_lock: {e}", file=sys.stderr)
        return 1
    pathlib.Path(a.out).write_text(text)
    print(f"wrote {a.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
