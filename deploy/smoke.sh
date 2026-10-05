#!/bin/sh
# Smoke test for deploy/compose: assumes `docker compose up -d` already ran.
# Polls (services start asynchronously), so it is safe to run immediately.
set -eu
ENV_FILE="${COMPOSE_ENV_FILE:-deploy/compose/.env}"
# shellcheck disable=SC1090
. "$ENV_FILE"

fail() { echo "SMOKE FAIL: $*" >&2; exit 1; }

# retry <desc> <cmd...> — up to 60 s
retry() {
  desc=$1; shift
  i=0
  until "$@" >/dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -ge 60 ] && fail "$desc (gave up after 60 s)"
    sleep 1
  done
  echo "ok: $desc"
}

CTRL=http://127.0.0.1:9901
AGG=http://127.0.0.1:9902
UI=http://127.0.0.1:9903
GSP_ADMIN=http://127.0.0.1:9900

retry "controller /healthz" curl -fsS "$CTRL/healthz"
retry "aggregator /healthz" curl -fsS "$AGG/healthz"
retry "gsp admin /healthz" curl -fsS "$GSP_ADMIN/healthz"
retry "ui /healthz" curl -fsS "$UI/healthz"

# The seed step wrote revision 1 and gsp pulled it.
retry "controller serves the seeded config" \
  curl -fsS -H "Authorization: Bearer $GSP_CONTROLLER_TOKEN" "$CTRL/config"

# gsp pushed its state to the aggregator (proves --controller + --aggregator
# wiring and the aggregator token end to end).
fleet_has_gsp() {
  curl -fsS -H "Authorization: Bearer $GSP_AGGREGATOR_TOKEN" "$AGG/fleet/pools" |
    grep -q '"instance"'
}
retry "aggregator reports the gsp instance" fleet_has_gsp

# UI login works with the configured password.
ui_login() {
  curl -fsS -X POST -H 'content-type: application/json' \
    -d "{\"password\":\"$GSP_UI_PASSWORD\"}" "$UI/ui/login"
}
retry "ui login" ui_login

# gsp_build_info carries the commit the image was built from. `unknown` means the
# build had no commit (build-images.sh / release.yml didn't pass GSP_GIT_SHA).
has_commit() { # <name> <url> [bearer token]
  body=$(curl -fsS ${3:+-H "Authorization: Bearer $3"} "$2") || return 1
  echo "$body" | grep '^gsp_build_info' | grep -q 'commit="' || return 1
  ! echo "$body" | grep '^gsp_build_info' | grep -q 'commit="unknown"'
}
retry "controller gsp_build_info has a commit" has_commit controller "$CTRL/metrics" "$GSP_CONTROLLER_TOKEN"
retry "aggregator gsp_build_info has a commit" has_commit aggregator "$AGG/metrics" "$GSP_AGGREGATOR_TOKEN"
retry "gsp gsp_build_info has a commit" has_commit gsp "$GSP_ADMIN/metrics"

# A wrong token must be refused (the gate is actually on).
code=$(curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer wrong' "$CTRL/config")
[ "$code" = 401 ] || fail "controller accepted a wrong token (HTTP $code)"
echo "ok: controller rejects a wrong token"

echo "SMOKE PASS"
