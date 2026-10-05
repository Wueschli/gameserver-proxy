#!/bin/sh
# Builds every runtime target and proves each binary starts on the runtime
# base (catches a builder/runtime glibc mismatch). Prints image sizes.
# BIN_SOURCE=prebuilt takes binaries from deploy/prebuilt/ instead of compiling.
# WAYHOUSE_GIT_SHA (default: this checkout's HEAD) becomes `commit=` in wayhouse_build_info;
# a builder build is checked to carry it, since Docker has no .git to fall back on.
set -eu
cd "$(dirname "$0")/.."
sha=${WAYHOUSE_GIT_SHA:-$(git rev-parse --short=12 HEAD 2>/dev/null || true)}
sha=$(printf %.12s "$sha")   # the short form the binary reports (a full SHA is fine to pass)
src=${BIN_SOURCE:-builder}
for t in wayhouse wayhouse-minimal wayhouse-controller wayhouse-aggregator wayhouse-ui wayhouse-agent; do
  img="wayhouse-deploy/$t:local"
  docker build -f deploy/Dockerfile --build-arg BIN_SOURCE="$src" --build-arg WAYHOUSE_GIT_SHA="$sha" --target "$t" -t "$img" .
  docker run --rm "$img" --version
  if [ "$src" = builder ]; then
    sh deploy/check-image-commit.sh "$img" "$sha"
  fi
done
docker images --format '{{.Repository}}:{{.Tag}} {{.Size}}' | grep '^wayhouse-deploy/'
