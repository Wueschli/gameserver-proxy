#!/usr/bin/env python3
"""Checks that docs/upgrading.md's version table matches the code (#185).

The newest (first) row of the table in docs/upgrading.md must carry the current
`PROTOCOL_MAJOR.MINOR`, `CONFIG_SCHEMA_VERSION`, `STORE_FORMAT` and the sniffer
`ABI_MAJOR.ABI_MINOR`, parsed from the Rust source. A wire, schema or store
change that forgets its row fails here. The product version only warns (and `unreleased` is accepted for a row not yet tagged): it moves
with release-please, not with the change that bumped the protocol.

Usage: check_upgrading_doc.py [repo-root]
Tested by check_upgrading_doc_test.py.
"""
import re
import sys
import tomllib
from pathlib import Path

SOURCES = {
    "protocol": "crates/wayhouse-http/src/protocol.rs",
    "config": "crates/wayhouse-config/src/version.rs",
    "store": "crates/wayhouse-controller/src/store.rs",
    "abi": "crates/wayhouse-sniffer-abi/src/lib.rs",
    "cargo": "Cargo.toml",
}
# Column order of the table, and the constants key each column is compared with.
COLUMNS = ["product", "protocol", "config_schema", "store_format", "abi"]
LABELS = {
    "protocol": "protocol",
    "config_schema": "config schema",
    "store_format": "store format",
    "abi": "sniffer ABI",
}


def _const(src: str, name: str) -> str:
    m = re.search(rf"pub const {name}: u\d+ = (\d+);", src)
    if not m:
        raise ValueError(f"cannot find `pub const {name}`")
    return m.group(1)


def parse_constants(src: dict) -> dict:
    """`src` maps the SOURCES keys to file contents."""
    version = tomllib.loads(src["cargo"]).get("workspace", {}).get("package", {}).get("version")
    if not version:
        raise ValueError("Cargo.toml has no [workspace.package] version")
    return {
        "protocol": f"{_const(src['protocol'], 'PROTOCOL_MAJOR')}.{_const(src['protocol'], 'PROTOCOL_MINOR')}",
        "config_schema": _const(src["config"], "CONFIG_SCHEMA_VERSION"),
        "store_format": _const(src["store"], "STORE_FORMAT"),
        "abi": f"{_const(src['abi'], 'ABI_MAJOR')}.{_const(src['abi'], 'ABI_MINOR')}",
        "product": version,
    }


def _newest_row(doc: str):
    """The first data row of the first table whose header starts `| Release`."""
    lines = doc.splitlines()
    for i, line in enumerate(lines):
        if re.match(r"\|\s*Release\s*\|", line):
            for row in lines[i + 2 :]:  # skip the separator line
                if not row.startswith("|"):
                    return None
                return [c.strip() for c in row.strip().strip("|").split("|")]
    return None


def check(doc: str, consts: dict):
    """Returns (errors, warnings)."""
    row = _newest_row(doc)
    if row is None or len(row) < len(COLUMNS):
        return (["docs/upgrading.md has no version table (a `| Release | Protocol | ...` header and a row)"], [])
    errors, warnings = [], []
    for key, cell in zip(COLUMNS, row):
        if key == "product":
            if cell != "unreleased" and cell != consts["product"]:
                warnings.append(
                    f"newest table row is release {cell} but the workspace version is {consts['product']}"
                )
        elif cell != consts[key]:
            errors.append(
                f"newest table row says {LABELS[key]} {cell} but the code has {consts[key]}: "
                "add or update the row in docs/upgrading.md"
            )
    return errors, warnings


def main(root: Path) -> int:
    consts = parse_constants({k: (root / p).read_text(encoding="utf-8") for k, p in SOURCES.items()})
    errors, warnings = check((root / "docs/upgrading.md").read_text(encoding="utf-8"), consts)
    for w in warnings:
        print(f"warning: {w}")
    for e in errors:
        print(f"error: {e}")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(Path(sys.argv[1]) if len(sys.argv) > 1 else Path(".")))
