#!/bin/sh
# cargo audit over every Cargo.lock in the repo, one JSON report each in <dir>
# (rendered by audit_summary.py). Exit 1 if any lockfile has a vulnerability or
# cargo audit failed, so a local run tells you; CI runs it as continue-on-error.
#   sh .github/scripts/cargo_audit.sh [dir]     (default target/cargo-audit)
set -u
cd "$(dirname "$0")/../.."
out=${1:-target/cargo-audit}
rm -rf "$out" && mkdir -p "$out"
rc=0
for lock in Cargo.lock crates/sniffers/Cargo.lock crates/wayhouse-config/fuzz/Cargo.lock; do
  name=$(printf '%s' "$lock" | tr '/' '_' | sed 's/_Cargo.lock$//; s/^Cargo.lock$/root/')
  echo "== $lock"
  cargo audit --json --file "$lock" >"$out/$name.json" || rc=1
  [ -s "$out/$name.json" ] || { echo "$lock" >>"$out/not-audited.txt"; rc=1; }
done
exit $rc
