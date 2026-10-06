#!/usr/bin/env python3
"""Checks that a PR title is a conventional commit.

The squash-merge title becomes the commit on main and the input of the changelog
(release-please), so it has to parse. Advisory: pr-title.yml is not part of `ci-ok`.

Usage: check_pr_title.py "<title>"   (exit 0 valid, 1 invalid)
Tested by check_pr_title_test.py.
"""
import re
import sys

TITLE = re.compile(
    r"(feat|fix|perf|refactor|docs|test|build|ci|chore|revert)(\([a-z0-9._-]+\))?!?: \S.*"
)


def valid(title: str) -> bool:
    return len(title) <= 100 and TITLE.fullmatch(title) is not None


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    if not valid(sys.argv[1]):
        sys.exit(
            f"error: PR title {sys.argv[1]!r} is not a conventional commit "
            "(type(scope)!: summary, max 100 chars; types: feat fix perf refactor docs "
            "test build ci chore revert). The squash title is the changelog entry."
        )
