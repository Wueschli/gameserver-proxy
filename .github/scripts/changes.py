#!/usr/bin/env python3
"""Maps a changed-file list (stdin, one path per line) to the CI areas that need
to run. Prints `<area>=true|false` lines for $GITHUB_OUTPUT. `--all` forces
everything (schedule / manual dispatch / unknown diff).

Rust areas are derived from `cargo metadata`, not from hand-kept crate lists: a
changed file belongs to the Cargo package whose directory holds it, and a job
runs when that package is one of the packages the job builds or tests (its
ROOTS), or something they depend on through path dependencies, transitively.
A crate that gains a dependency is picked up with no edit here.

A Cargo.lock only affects its own workspace's members: the root lockfile can't
change what crates/wayhouse-config/fuzz (its own workspace and lockfile) builds,
while the root Cargo.toml can (wayhouse-config inherits from it).

Deliberately conservative: a workflow edit runs everything; the root
Cargo.toml, the toolchain file or .cargo/ runs every Rust area; a path under
crates/ that no package owns (a new or half-registered crate) runs every Rust
area; if cargo metadata fails, or a ROOTS entry matches no package (a rename),
everything runs. What metadata can't see: a crate reading another
crate's files at compile or test time (include_bytes!, fixtures). Today that is
wayhouse-fleet-tests reading wayhouse-http's test fixtures, and wayhouse-http is already in
every job wayhouse-fleet-tests is. Keep ROOTS in sync with what each job in ci.yml
builds or tests. Tested by changes_test.py.
"""
import json
import os
import re
import subprocess
import sys
from typing import Dict, FrozenSet, List, NamedTuple, Optional, Set

AREAS = ("ui", "plugins", "tunnel", "deploy", "fuzz", "release")

# The packages each Rust job builds or tests. A name, or a directory ending in
# "/" for every package under it.
ROOTS = {
    # crates/plugins (wasm) + `cargo nextest -p wayhouse` loading the built plugins.
    "plugins": ["wayhouse", "crates/plugins/"],
    # make tunnel-e2e-ci: debug-builds the five binaries, runs wayhouse-fleet-tests.
    "tunnel": ["wayhouse", "wayhouse-agent", "wayhouse-controller", "wayhouse-aggregator", "wayhouse-ui", "wayhouse-fleet-tests"],
    # crates/wayhouse-config/fuzz (its own workspace; depends on wayhouse-config).
    "fuzz": ["wayhouse-config-fuzz"],
}

# The root manifest (inherited by members of every workspace), the toolchain and
# cargo config affect every Rust job.
RUSTWIDE = re.compile(r"^(Cargo\.toml|rust-toolchain\.toml)$|^\.cargo/")

# A change touching only these needs no code job (test, audit, ...); they only
# exist as a PR gate. Mirrors the push trigger's paths-ignore in ci.yml.
DOCS_ONLY = re.compile(r"\.md$|^docs/|^LICENSE-")

# Non-Cargo paths per area, on top of the package graph.
EXTRA = {
    "ui": re.compile(r"^crates/wayhouse-ui/web/"),
    # .config/nextest.toml holds the `ci` profile both jobs run under.
    "plugins": re.compile(r"^\.config/"),
    "tunnel": re.compile(r"^Makefile$|^\.config/"),
    # The UI's npm lockfile is here too: those packages end up in the wayhouse-ui
    # image's bundle, and the deploy job's Trivy scan checks them.
    "deploy": re.compile(
        r"^deploy/|^\.dockerignore$|^Makefile$|^Cargo\.(toml|lock)$"
        r"|^crates/wayhouse-ui/web/package-lock\.json$|^\.trivyignore$"
    ),
}

# Inside a crate's directory but not part of its build (standalone npm project).
NOT_CARGO = ("crates/wayhouse-ui/web/",)


class Package(NamedTuple):
    name: str
    deps: FrozenSet[str]  # manifest dirs of its path dependencies


class Graph(NamedTuple):
    packages: Dict[str, Package]  # manifest dir (repo-relative) -> package
    workspaces: Dict[str, List[str]]  # workspace root dir ("" = repo) -> member dirs


class MetadataError(Exception):
    pass


def _cargo_metadata(repo: str, manifest: str) -> dict:
    try:
        out = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1",
             "--manifest-path", os.path.join(repo, manifest)],
            check=True, capture_output=True, text=True,
        ).stdout
        return json.loads(out)
    except (OSError, subprocess.CalledProcessError, ValueError) as e:
        detail = getattr(e, "stderr", "") or str(e)
        raise MetadataError(f"cargo metadata for {manifest}: {detail.strip()}") from e


def load_graph(repo: str) -> Graph:
    """Every Cargo package in the repo, across the root and nested workspaces."""
    try:
        listed = subprocess.run(
            ["git", "-C", repo, "ls-files", "--", "Cargo.toml", "*/Cargo.toml"],
            check=True, capture_output=True, text=True,
        ).stdout.split()
    except (OSError, subprocess.CalledProcessError) as e:
        raise MetadataError(f"git ls-files: {e}") from e
    if not listed:
        raise MetadataError("no Cargo.toml found")
    real = os.path.realpath(repo)

    def rel(path: str) -> str:
        r = os.path.relpath(os.path.realpath(path), real)
        return "" if r == "." else r

    packages: Dict[str, Package] = {}
    workspaces: Dict[str, List[str]] = {}
    # Shallowest first, so one metadata call covers a whole workspace.
    for manifest in sorted(listed, key=lambda m: (m.count("/"), m)):
        d = os.path.dirname(manifest)
        if d in packages or d in workspaces:
            continue
        meta = _cargo_metadata(repo, manifest)
        members = []
        for p in meta["packages"]:
            pdir = rel(os.path.dirname(p["manifest_path"]))
            deps = frozenset(rel(x["path"]) for x in p["dependencies"] if x.get("path"))
            packages[pdir] = Package(p["name"], deps)
            members.append(pdir)
        workspaces[rel(meta["workspace_root"])] = members
    return Graph(packages, workspaces)


def _under(path: str, d: str) -> bool:
    return d == "" or path.startswith(d + "/")


def owners(path: str, g: Graph) -> Optional[Set[str]]:
    """The package dirs a changed file belongs to; None if no package owns it."""
    if any(path.startswith(n) for n in NOT_CARGO):
        return set()
    best = max((d for d in g.packages if d and _under(path, d)), key=len, default=None)
    if best is not None:
        return {best}
    # A nested workspace's own files (its Cargo.toml, README): all its members.
    # The repo root's own files are RUSTWIDE's / EXTRA's job.
    ws = max((d for d in g.workspaces if d and _under(path, d)), key=len, default=None)
    if ws is not None:
        return set(g.workspaces[ws])
    return None


def dependents(changed: Set[str], g: Graph) -> Set[str]:
    """`changed` plus every package that depends on one of them, transitively."""
    hit = set(changed)
    grew = True
    while grew:
        grew = False
        for d, p in g.packages.items():
            if d not in hit and p.deps & hit:
                hit.add(d)
                grew = True
    return hit


def _roots(area: str, g: Graph) -> Set[str]:
    out = set()
    for r in ROOTS[area]:
        if r.endswith("/"):
            found = {d for d in g.packages if _under(d + "/", r.rstrip("/"))}
        else:
            found = {d for d, p in g.packages.items() if p.name == r}
        if not found:
            raise MetadataError(f"ROOTS[{area!r}] entry {r!r} matches no Cargo package")
        out |= found
    return out


def areas(files: List[str], g: Graph) -> Dict[str, bool]:
    files = [f for f in files if f]
    if any(f.startswith(".github/") for f in files):
        return {a: True for a in AREAS}
    # Documentation builds and scans nothing, even under a scoped dir (deploy/README.md).
    files = [f for f in files if not DOCS_ONLY.search(f)]
    out = {a: any(rx.search(f) for f in files) for a, rx in EXTRA.items()}
    rustwide = any(RUSTWIDE.search(f) for f in files)
    changed: Set[str] = set()
    locked: Set[str] = set()  # members of a workspace whose Cargo.lock changed
    for f in files:
        d, base = os.path.split(f)
        if base == "Cargo.lock" and d in g.workspaces:
            locked |= set(g.workspaces[d])
            continue
        o = owners(f, g)
        if o is None and f.startswith("crates/"):
            rustwide = True  # unknown crate path: don't guess
        changed |= o or set()
    hit = dependents(changed, g) | locked
    for a in ROOTS:
        out[a] = out.get(a, False) or rustwide or bool(_roots(a, g) & hit)
    # The shared release build (build-release job) feeds plugins and deploy.
    out["release"] = out["plugins"] or out["deploy"]
    return {a: out.get(a, False) for a in AREAS}


# What the `docs` CI job (Prettier and the link check) depends on. The Makefile holds the
# Prettier pin. `.prettierrc.json` and `.prettierignore` are not in DOCS_ONLY on purpose:
# they are config, so a PR touching only them also runs the code jobs.
DOCS_JOB = re.compile(r"\.md$|^\.prettier(rc\.json|ignore)$|^Makefile$|^\.github/")


def is_docs(files: List[str]) -> bool:
    """True when a changed file is Markdown or the tooling that checks it."""
    return any(f and DOCS_JOB.search(f) for f in files)


def is_code(files: List[str]) -> bool:
    """False when every changed file is documentation (or the list is empty)."""
    return any(f and not DOCS_ONLY.search(f) for f in files)


def main(argv, stdin, stdout, stderr, loader=load_graph) -> None:
    if "--all" in argv:
        result = {a: True for a in AREAS}
        code = docs = True
    else:
        files = stdin.read().splitlines()
        code = is_code(files)
        docs = is_docs(files)
        try:
            result = areas(files, loader(os.getcwd()))
        except MetadataError as e:
            # A GitHub annotation, so a broken detector isn't just a silently full run.
            print(f"::warning title=change detection::{e}; running every job", file=stderr)
            result = {a: True for a in AREAS}
            code = docs = True
    for a in AREAS:
        print(f"{a}={'true' if result[a] else 'false'}", file=stdout)
    print(f"code={'true' if code else 'false'}", file=stdout)
    print(f"docs={'true' if docs else 'false'}", file=stdout)


if __name__ == "__main__":
    main(sys.argv[1:], sys.stdin, sys.stdout, sys.stderr)
