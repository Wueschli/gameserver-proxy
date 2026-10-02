#!/bin/sh
# Maps a changed-file list (stdin, one path per line) to the CI areas that need
# to run. Prints `<area>=true|false` lines for $GITHUB_OUTPUT. `--all` forces
# everything (schedule / manual dispatch / unknown diff).
# Tested by changes_test.sh. Keep the patterns in sync with the job list in
# ci.yml; a workflow edit runs everything.
set -eu
files=$(cat)

if [ "${1:-}" = "--all" ] || printf '%s\n' "$files" | grep -qE '^\.github/'; then
  for a in ui plugins tunnel deploy fuzz release; do echo "$a=true"; done
  exit 0
fi

# any <ERE> over the changed files
touches() { printf '%s\n' "$files" | grep -qE "$1"; }
out() { if touches "$2"; then echo "$1=true"; else echo "$1=false"; fi; }

# Cargo.* / toolchain changes affect every Rust job.
RUSTWIDE='^(Cargo\.(toml|lock)|rust-toolchain\.toml)$|^\.cargo/'

PLUGINS="^crates/(plugins|gsp|gsp-core|gsp-config|gsp-http)/|$RUSTWIDE"
# The UI's npm lockfile is in DEPLOY too: those packages end up in the gsp-ui image's
# bundle, and the deploy job's Trivy scan checks them.
DEPLOY='^deploy/|^\.dockerignore$|^Makefile$|^Cargo\.(toml|lock)$|^crates/gsp-ui/web/package-lock\.json$|^\.trivyignore$'

out ui      '^crates/gsp-ui/web/'
out plugins "$PLUGINS"
out tunnel  "^crates/(gsp|gsp-core|gsp-config|gsp-http|gsp-agent|gsp-controller|gsp-aggregator|gsp-fleet-tests)/|^crates/gsp-ui/(src|tests|Cargo\.toml|build\.rs)|^Makefile$|$RUSTWIDE"
out deploy  "$DEPLOY"
out fuzz    "^crates/gsp-config/|$RUSTWIDE"
# The shared release build (build-release job) feeds plugins and deploy.
out release "$PLUGINS|$DEPLOY"
