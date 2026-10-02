#!/bin/sh
# Trivy over everything the deploy/ images ship. Informational: CI runs it as a
# non-blocking step and renders the reports on the run page
# (.github/scripts/trivy_summary.py). Locally the exit code is 1 if anything was
# found, so you can tell. Run after build-images.sh (needs the
# gsp-deploy/<t>:local images, the Docker daemon and `trivy` on PATH; CI installs
# a pinned, checksum-verified release — see the deploy job).
#
# Reports a HIGH or CRITICAL vulnerability that has a fix available, or a secret:
#   - in each image: its OS packages (the distroless base) and any secrets;
#   - in the lockfiles the images were built from: Cargo.lock (every binary is
#     built --locked from it; a plain release binary carries no dependency list,
#     so the image scan can't see its crates) and the UI's package-lock.json.
# JSON and SARIF reports land in $TRIVY_REPORT_DIR (default target/trivy).
# With $TRIVY_IMAGE_DIR set, the images come from `docker save` tarballs there
# (<target>.tar) instead of the Docker daemon — CI's trivy job runs that way.
# Accepted findings go in .trivyignore at the repo root, each with a reason.
set -eu
cd "$(dirname "$0")/.."
command -v trivy >/dev/null || { echo "trivy not found on PATH" >&2; exit 2; }
out=${TRIVY_REPORT_DIR:-target/trivy}
rm -rf "$out" && mkdir -p "$out"
set -- --exit-code 1 --severity "${TRIVY_SEVERITY:-HIGH,CRITICAL}" --ignore-unfixed \
  --ignorefile .trivyignore --no-progress --format json
rc=0
# scan <report-name> <trivy subcommand + target...>
scan() {
  name=$1; shift
  echo "== $name"
  trivy "$@" --output "$out/$name.json" || rc=1
  if [ -s "$out/$name.json" ]; then
    trivy convert --format table "$out/$name.json"
    trivy convert --format sarif --output "$out/$name.sarif" "$out/$name.json"
  else
    echo "$name" >>"$out/not-scanned.txt" # an error, not findings: no report at all
  fi
}
for t in gsp gsp-controller gsp-aggregator gsp-ui gsp-agent; do
  if [ -n "${TRIVY_IMAGE_DIR:-}" ]; then
    scan "image-$t" image "$@" --scanners vuln,secret --input "$TRIVY_IMAGE_DIR/$t.tar"
  else
    # --image-src docker: only the image just built, never a same-named registry one.
    scan "image-$t" image "$@" --image-src docker --scanners vuln,secret "gsp-deploy/$t:local"
  fi
done
scan lock-cargo fs "$@" --scanners vuln Cargo.lock
scan lock-ui-npm fs "$@" --scanners vuln crates/gsp-ui/web/package-lock.json
exit $rc
