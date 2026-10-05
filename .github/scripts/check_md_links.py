#!/usr/bin/env python3
"""Fails when a relative Markdown link points at a file or `#anchor` that does not exist.

Usage: check_md_links.py <file.md>...   (prints `file:line: broken link target <t>`, exit 1)
External (`scheme:`) links are not fetched. Links inside fenced code blocks and inline code
are ignored. Anchors are GitHub heading slugs. Stdlib only; tested by check_md_links_test.py.
"""
import os
import re
import sys
from typing import List, Set

LINK = re.compile(r"\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
HEADING = re.compile(r"^ {0,3}#{1,6}\s+(.*?)\s*#*\s*$")
INLINE_CODE = re.compile(r"`[^`]*`")


def slug(heading: str) -> str:
    """GitHub's heading anchor: lowercase, drop punctuation, spaces to dashes."""
    h = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", heading.strip().lower())  # links keep their label
    h = re.sub(r"[`*]", "", h)
    h = re.sub(r"(?<!\w)_|_(?!\w)", "", h)  # emphasis underscores, not foo_bar
    h = re.sub(r"[^\w\- ]", "", h)
    return h.replace(" ", "-")


def _lines(path: str):
    """Yields (line number, text) outside fenced code blocks."""
    fence = None
    with open(path, encoding="utf-8") as f:
        for n, line in enumerate(f, 1):
            m = re.match(r"^ {0,3}(`{3,}|~{3,})(.*)$", line)
            if m:
                run, rest = m.group(1), m.group(2)
                if fence is None:
                    fence = run
                elif run[0] == fence[0] and len(run) >= len(fence) and not rest.strip():
                    fence = None
                continue
            if fence is None:
                yield n, line


def anchors(path: str) -> Set[str]:
    seen, out = {}, set()
    for _, line in _lines(path):
        m = HEADING.match(line)
        if m:
            s = slug(m.group(1))
            k = seen.get(s, 0)
            seen[s] = k + 1
            out.add(s if k == 0 else f"{s}-{k}")
    return out


def check_files(paths: List[str]) -> List[str]:
    errors = []
    cache = {}
    for path in paths:
        for n, line in _lines(path):
            for m in LINK.finditer(INLINE_CODE.sub("", line)):
                target = m.group(1)
                if re.match(r"^[A-Za-z][A-Za-z0-9+.-]*:", target):
                    continue
                file_part, _, frag = target.partition("#")
                dest = path if not file_part else os.path.normpath(
                    os.path.join(os.path.dirname(path), file_part))
                bad = not os.path.exists(dest)
                if not bad and frag and dest.endswith(".md") and os.path.isfile(dest):
                    if dest not in cache:
                        cache[dest] = anchors(dest)
                    bad = frag.lower() not in cache[dest]
                if bad:
                    errors.append(f"{path}:{n}: broken link target {target}")
    return errors


def main(argv: List[str]) -> int:
    errors = check_files(argv[1:])
    for e in errors:
        print(e)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
