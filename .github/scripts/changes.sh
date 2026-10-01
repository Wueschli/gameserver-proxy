#!/bin/sh
# Maps a changed-file list (stdin, one path per line) to the CI areas that need
# to run. Prints `<area>=true|false` lines for $GITHUB_OUTPUT. `--all` forces
# everything (schedule / manual dispatch / unknown diff).
# Tested by changes_test.sh. Keep the patterns in sync with the job list in
# ci.yml; a workflow edit runs everything.
set -eu
files=$(cat)

if [ "${1:-}" = "--all" ] || printf '%s\n' "$files" | grep -qE '^\.github/'; then
  for a in ui plugins tunnel deploy fuzz; do echo "$a=true"; done
  exit 0
fi

# any <ERE> over the changed files
touches() { printf '%s\n' "$files" | grep -qE "$1"; }
out() { if touches "$2"; then echo "$1=true"; else echo "$1=false"; fi; }

# Cargo.* / toolchain changes affect every Rust job.
RUSTWIDE='^(Cargo\.(toml|lock)|rust-toolchain\.toml)$|^\.cargo/'

out ui      '^crates/gsp-ui/web/'
out plugins "^crates/(plugins|gsp|gsp-core|gsp-config)/|$RUSTWIDE"
out tunnel  "^crates/(gsp|gsp-core|gsp-config|gsp-agent|gsp-controller|gsp-aggregator|gsp-fleet-tests)/|^crates/gsp-ui/(src|tests|Cargo\.toml|build\.rs)|^Makefile$|$RUSTWIDE"
out deploy  '^deploy/|^\.dockerignore$|^Makefile$|^Cargo\.(toml|lock)$'
out fuzz    "^crates/gsp-config/|$RUSTWIDE"
