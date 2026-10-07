#!/usr/bin/env python3
"""Download the official sniffers pinned in sniffers.lock for the e2e tests.

    python3 .github/scripts/fetch_sniffers.py [--lock sniffers.lock] [--out target/sniffers]

Each `[sniffers.<name>]` entry names a release tag, a file and the sha256 the file must
have. Files are fetched from `https://github.com/<repo>/releases/download/<tag>/<file>`
over https only (a redirect to anything but https is refused), written atomically, and
kept only when the hash matches. A file already in --out with the right hash is not
fetched again. Every failure is reported, then the exit status is non-zero. Prints the
output directory (what WAYHOUSE_SNIFFERS_DIR should point at).

`--allow-insecure-test-url BASE` replaces the release URL prefix with a plain-http
server, for the script's own tests; it works only with WAYHOUSE_FETCH_TEST=1 in the
environment.
"""
import argparse
import hashlib
import os
import pathlib
import re
import sys
import tempfile
import tomllib
import urllib.error
import urllib.request

HEX64 = re.compile(r"^[0-9a-f]{64}$")
SAFE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
MAX_BYTES = 8 * 1024 * 1024  # the proxy's module size cap


class HttpsOnly(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        if not newurl.lower().startswith("https://"):
            raise urllib.error.URLError(f"refusing redirect to a non-https URL: {newurl}")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def sha256_of(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load_lock(path):
    lock = tomllib.loads(pathlib.Path(path).read_text())
    repo = lock.get("repo", "")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        raise ValueError(f"lock: bad repo {repo!r}")
    entries = {}
    for name, e in lock.get("sniffers", {}).items():
        if not SAFE.fullmatch(name):
            raise ValueError(f"lock: bad sniffer name {name!r}")
        tag, file, digest = e.get("tag", ""), e.get("file", ""), e.get("sha256", "")
        if not SAFE.fullmatch(tag) or not SAFE.fullmatch(file):
            raise ValueError(f"lock: {name}: tag and file must be plain names, got {tag!r}, {file!r}")
        if not HEX64.fullmatch(digest):
            raise ValueError(f"lock: {name}: sha256 must be 64 lowercase hex characters")
        entries[name] = (tag, file, digest)
    if not entries:
        raise ValueError("lock: no [sniffers.*] entries")
    return repo, entries


def download(url, opener):
    with opener.open(url, timeout=60) as r:
        data = r.read(MAX_BYTES + 1)
    if len(data) > MAX_BYTES:
        raise ValueError("larger than 8 MiB")
    return data


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--lock", default="sniffers.lock")
    ap.add_argument("--out", default="target/sniffers")
    ap.add_argument("--base-url", help=argparse.SUPPRESS)
    ap.add_argument("--allow-insecure-test-url", help=argparse.SUPPRESS)
    args = ap.parse_args()

    insecure = args.allow_insecure_test_url
    if insecure and os.environ.get("WAYHOUSE_FETCH_TEST") != "1":
        print("--allow-insecure-test-url needs WAYHOUSE_FETCH_TEST=1", file=sys.stderr)
        return 2
    try:
        repo, entries = load_lock(args.lock)
    except (OSError, ValueError, tomllib.TOMLDecodeError) as e:
        print(f"{args.lock}: {e}", file=sys.stderr)
        return 2
    base = insecure or args.base_url or f"https://github.com/{repo}/releases/download"
    if not base.startswith("https://") and not insecure:
        print(f"refusing a non-https base URL: {base}", file=sys.stderr)
        return 2
    opener = urllib.request.build_opener(HttpsOnly)
    out = pathlib.Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    failures = []
    for name, (tag, file, digest) in sorted(entries.items()):
        dest = out / file
        if dest.is_file() and sha256_of(dest) == digest:
            print(f"{name}: already present", file=sys.stderr)
            continue
        url = f"{base.rstrip('/')}/{tag}/{file}"
        try:
            data = download(url, opener)
        except (OSError, ValueError) as e:  # URLError and HTTPError are OSErrors
            failures.append(f"{name}: cannot fetch {url}: {e}")
            continue
        got = hashlib.sha256(data).hexdigest()
        if got != digest:
            failures.append(f"{name}: sha256 mismatch for {url}: lock has {digest}, got {got}")
            continue
        part = None
        try:
            with tempfile.NamedTemporaryFile(dir=out, delete=False, suffix=".part") as t:
                part = t.name
                t.write(data)
            os.replace(part, dest)
        except OSError as e:
            if part:
                pathlib.Path(part).unlink(missing_ok=True)
            failures.append(f"{name}: cannot write {dest}: {e}")
            continue
        print(f"{name}: fetched {len(data)} bytes", file=sys.stderr)

    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print(out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
