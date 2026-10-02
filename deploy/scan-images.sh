#!/bin/sh
# Trivy over everything the deploy/ images ship; fails on a HIGH or CRITICAL
# vulnerability that has a fixed version available. Run after build-images.sh
# (needs the gsp-deploy/<t>:local images, the Docker daemon and `trivy` on PATH;
# CI installs a pinned, checksum-verified release — see the deploy job).
#
# Two kinds of scan, because a plain release binary carries no dependency list:
#   - each image: its OS packages (the distroless base) and any secrets;
#   - the lockfiles the images were built from: Cargo.lock (every binary is
#     built --locked from it) and the UI's package-lock.json (its bundle).
# Accepted findings go in .trivyignore at the repo root, one ID per line with a
# comment saying why and until when.
set -eu
cd "$(dirname "$0")/.."
command -v trivy >/dev/null || { echo "trivy not found on PATH" >&2; exit 2; }
severity=${TRIVY_SEVERITY:-HIGH,CRITICAL}
set -- --exit-code 1 --severity "$severity" --ignore-unfixed --ignorefile .trivyignore --no-progress
rc=0
for t in gsp gsp-controller gsp-aggregator gsp-ui gsp-agent; do
  echo "== image gsp-deploy/$t:local"
  trivy image "$@" --scanners vuln,secret "gsp-deploy/$t:local" || rc=1
done
for lock in Cargo.lock crates/gsp-ui/web/package-lock.json; do
  echo "== $lock"
  trivy fs "$@" --scanners vuln "$lock" || rc=1
done
exit $rc
