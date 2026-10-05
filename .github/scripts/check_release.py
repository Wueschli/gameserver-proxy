#!/usr/bin/env python3
"""Refuses a release tag that disagrees with the workspace version.

The images are labelled and tagged from the git tag, but every binary reports
`[workspace.package] version` from Cargo.toml (`--version`, `gsp_build_info`).
`v0.1.0` on a tree that still says 0.0.1 would publish `:0.1.0` images whose
binaries say 0.0.1, so release.yml runs this first.

Usage: check_release.py <tag> [Cargo.toml]   (prints the version on success)
Tested by check_release_test.py.
"""
import re
import sys
import tomllib

TAG = re.compile(r"v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?")


def check(tag: str, cargo_toml: str) -> str:
    """Returns the version; raises ValueError describing the mismatch."""
    if not TAG.fullmatch(tag):
        raise ValueError(f"tag {tag!r} is not vMAJOR.MINOR.PATCH[-prerelease]")
    with open(cargo_toml, "rb") as f:
        version = tomllib.load(f).get("workspace", {}).get("package", {}).get("version")
    if not version:
        raise ValueError(f"{cargo_toml} has no [workspace.package] version")
    if tag != f"v{version}":
        raise ValueError(
            f"tag {tag} does not match the workspace version {version}: "
            f"set version = \"{tag[1:]}\" in {cargo_toml} (and Cargo.lock), then re-tag"
        )
    return version


if __name__ == "__main__":
    if len(sys.argv) not in (2, 3):
        sys.exit(__doc__)
    try:
        print(check(sys.argv[1], sys.argv[2] if len(sys.argv) == 3 else "Cargo.toml"))
    except ValueError as e:
        sys.exit(f"error: {e}")
