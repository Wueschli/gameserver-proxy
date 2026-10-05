#!/bin/sh
# check-image-commit.sh <image> <sha>: fail unless the image's binary has <sha>
# compiled in (the commit= label of gsp_build_info, crates/gsp-http/build.rs).
# The images are distroless (no shell), so copy the binary out and look for it.
set -eu
img=$1 sha=$2
[ -n "$sha" ] || { echo "check-image-commit: no commit to check ($img)" >&2; exit 1; }
cid=$(docker create "$img")
trap 'docker rm -f "$cid" >/dev/null' EXIT
bin=$(docker inspect --format '{{index .Config.Entrypoint 0}}' "$img")
docker cp "$cid:$bin" - | grep -aqF "$sha" ||
  { echo "check-image-commit: $img does not carry commit $sha" >&2; exit 1; }
echo "ok: $img carries commit $sha"
