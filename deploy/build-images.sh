#!/bin/sh
# Builds every runtime target and proves each binary starts on the runtime
# base (catches a builder/runtime glibc mismatch). Prints image sizes.
set -eu
cd "$(dirname "$0")/.."
for t in gsp gsp-controller gsp-aggregator gsp-ui gsp-agent; do
  docker build -f deploy/Dockerfile --target "$t" -t "gsp-deploy/$t:local" .
  docker run --rm "gsp-deploy/$t:local" --version
done
docker images --format '{{.Repository}}:{{.Tag}} {{.Size}}' | grep '^gsp-deploy/'
