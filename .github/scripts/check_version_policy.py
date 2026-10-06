#!/usr/bin/env python3
"""Enforces the version policy (RELEASING.md, "Versioning").

- `[workspace.package] version` is MAJOR.MINOR.PATCH[-prerelease] and MAJOR is 0.
  Going to 1.0 is the maintainer's explicit call: it needs a reviewed edit of this
  script (there is deliberately no environment switch the workflows could carry);
  no tool, release PR or bot may get there on its own.
- Cargo.lock agrees with Cargo.toml for every workspace member, so a version bump
  that forgot `cargo update --workspace` fails here instead of in `cargo --locked`.

Usage: check_version_policy.py [Cargo.toml] [Cargo.lock]   (prints the version on success)
Tested by check_version_policy_test.py.
"""
import re
import sys
import tomllib

VERSION = re.compile(r"([0-9]+)\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?")


def check(cargo_toml_text: str, cargo_lock_text: str, allow_major: bool = False) -> str:
    """Returns the version; raises ValueError describing the violation."""
    ws = tomllib.loads(cargo_toml_text).get("workspace", {})
    version = ws.get("package", {}).get("version")
    if not version:
        raise ValueError("Cargo.toml has no [workspace.package] version")
    m = VERSION.fullmatch(version)
    if not m:
        raise ValueError(f"version {version!r} is not MAJOR.MINOR.PATCH[-prerelease]")
    if m.group(1) != "0" and not allow_major:
        raise ValueError(
            f"version {version} is not 0.x: versions stay 0.x until the maintainer "
            "decides on 1.0 (RELEASING.md, Versioning). Revert the bump; the maintainer "
            "moves to 1.0 deliberately by editing this script."
        )
    members = {p.rstrip("/").rsplit("/", 1)[-1] for p in ws.get("members", [])}
    locked = {
        p["name"]: p["version"]
        for p in tomllib.loads(cargo_lock_text).get("package", [])
        if "source" not in p and p["name"] in members
    }
    for name in sorted(members):
        if name not in locked:
            raise ValueError(
                f"Cargo.lock has no entry for workspace member {name}: "
                "run 'cargo update --workspace' and commit Cargo.lock"
            )
        if locked[name] != version:
            raise ValueError(
                f"Cargo.lock says {locked[name]} for {name}, Cargo.toml says {version}: "
                "run 'cargo update --workspace' and commit Cargo.lock"
            )
    return version


if __name__ == "__main__":
    if len(sys.argv) > 3:
        sys.exit(__doc__)
    toml_path = sys.argv[1] if len(sys.argv) > 1 else "Cargo.toml"
    lock_path = sys.argv[2] if len(sys.argv) > 2 else "Cargo.lock"
    try:
        with open(toml_path) as t, open(lock_path) as l:
            print(check(t.read(), l.read()))
    except ValueError as e:
        sys.exit(f"error: {e}")
