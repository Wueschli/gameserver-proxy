#!/bin/sh
# Tests for changes.sh. Each case: a changed-file list -> the exact flag set.
set -u
cd "$(dirname "$0")"
fails=0
# check <name> <files (newline separated)> <expected "ui plugins tunnel deploy fuzz" as 0/1 string> [--all]
check() {
  name=$1; files=$2; want=$3; flag=${4:-}
  # release (the shared release build) is needed iff plugins or deploy is.
  case "$want" in *plugins=true*|*deploy=true*) want="$want release=true" ;; *) want="$want release=false" ;; esac
  got=$(printf '%s\n' "$files" | sh ./changes.sh $flag | sort | tr '\n' ' ')
  exp=$(printf '%s' "$want" | tr ' ' '\n' | sort | tr '\n' ' ')
  if [ "$got" = "$exp" ]; then echo "ok:   $name"; else echo "FAIL: $name"; echo "  got  $got"; echo "  want $exp"; fails=$((fails+1)); fi
}
NONE="ui=false plugins=false tunnel=false deploy=false fuzz=false"
check "docs only changes nothing"        "README.md
docs/12-deployment.md" "$NONE"
check "ui web only"                      "crates/gsp-ui/web/src/App.tsx" "ui=true plugins=false tunnel=false deploy=false fuzz=false"
check "gsp-core: plugins+tunnel"         "crates/gsp-core/src/pool.rs" "ui=false plugins=true tunnel=true deploy=false fuzz=false"
check "gsp-config: also fuzz"            "crates/gsp-config/src/lib.rs" "ui=false plugins=true tunnel=true deploy=false fuzz=true"
check "gsp-http: plugins+tunnel"         "crates/gsp-http/src/lib.rs" "ui=false plugins=true tunnel=true deploy=false fuzz=false"
check "plugins crate only"               "crates/plugins/a2s/src/lib.rs" "ui=false plugins=true tunnel=false deploy=false fuzz=false"
check "gsp-agent: tunnel only"           "crates/gsp-agent/src/main.rs" "ui=false plugins=false tunnel=true deploy=false fuzz=false"
check "deploy dir only"                  "deploy/compose/gsp.yaml" "ui=false plugins=false tunnel=false deploy=true fuzz=false"
check "Dockerfile ignore"                ".dockerignore" "ui=false plugins=false tunnel=false deploy=true fuzz=false"
check "Cargo.lock: all rust + deploy"    "Cargo.lock" "ui=false plugins=true tunnel=true deploy=true fuzz=true"
check "Makefile: tunnel + deploy"        "Makefile" "ui=false plugins=false tunnel=true deploy=true fuzz=false"
check "workflow edit runs everything"    ".github/workflows/ci.yml" "ui=true plugins=true tunnel=true deploy=true fuzz=true"
check "mixed: ui web + deploy"           "crates/gsp-ui/web/package.json
deploy/Dockerfile" "ui=true plugins=false tunnel=false deploy=true fuzz=false"
check "--all (schedule/dispatch)"        "" "ui=true plugins=true tunnel=true deploy=true fuzz=true" --all
check "empty list is nothing, not all"   "" "$NONE"
[ "$fails" -eq 0 ] && echo "changes_test: all passed" || { echo "changes_test: $fails failed"; exit 1; }
