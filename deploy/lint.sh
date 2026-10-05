#!/bin/sh
# Static checks on deploy/ that need Docker's CLI but no daemon (compose config
# only renders). Each check pins a failure that only shows up on a real
# `docker compose up` / cluster — see docs/superpowers/plans/2026-10-01-deploy.md.
set -eu
cd "$(dirname "$0")/.."
# Render the defaults: CI's deploy job exports BIN_SOURCE=prebuilt, which must not
# change what these checks see.
unset BIN_SOURCE
export GSP_CONTROLLER_TOKEN=x GSP_AGGREGATOR_TOKEN=x GSP_INGEST_TOKEN=y GSP_UI_PASSWORD=x \
       GSP_TUNNEL_ENDPOINT=1.2.3.4:51820
C=deploy/compose

base=$(docker compose -f $C/docker-compose.yml config --format json)
tunnel=$(docker compose -f $C/docker-compose.yml -f $C/compose.tunnel.yml \
         --profile origin-demo config --format json)
smoke_plan=$(make -n deploy-smoke)

BASE="$base" TUNNEL="$tunnel" SMOKE_PLAN="$smoke_plan" ruby -rjson -ryaml -e '
errs = []
base   = JSON.parse(ENV["BASE"])["services"]
tunnel = JSON.parse(ENV["TUNNEL"])["services"]

# curl 8.11.1 (the pinned seed image) rejects `--opt=value`; values must be
# separate arguments or `seed` exits 2 and gsp never starts.
bad = base["seed"]["command"].grep(/\A--[a-z-]+=/)
errs << "seed passes --opt=value args to curl: #{bad.inspect}" unless bad.empty?

# gsp must report a unique instance name (default = admin listen addr).
errs << "compose gsp lacks --aggregator-instance" unless base["gsp"]["command"].any? { |a| a.start_with?("--aggregator-instance=") }
ds = YAML.load_stream(File.read("deploy/k8s/50-gsp-daemonset.yaml")).find { |d| d["kind"] == "DaemonSet" }
c = ds["spec"]["template"]["spec"]["containers"][0]
errs << "k8s gsp DaemonSet lacks serviceAccountName gsp (45-gsp-rbac.yaml)" unless ds.dig("spec", "template", "spec", "serviceAccountName") == "gsp"
errs << "k8s gsp lacks --aggregator-instance=$(NODE_NAME)" unless c["args"].include?("--aggregator-instance=$(NODE_NAME)")
errs << "k8s gsp lacks NODE_NAME from spec.nodeName" unless c["env"].any? { |e| e["name"] == "NODE_NAME" && e.dig("valueFrom", "fieldRef", "fieldPath") == "spec.nodeName" }

# NET_ADMIN is only in the bounding set for a non-root user: tunnel services run as root.
%w[gsp agent].each { |s| errs << "tunnel #{s} must run as root (user: \"0\")" unless tunnel[s]["user"] == "0" }
# The agent must not clash with gsp on the shared host network.
errs << "tunnel agent needs its own --listen-port" unless tunnel["agent"]["command"].include?("--listen-port=51821")
errs << "tunnel agent should sit in the origin-demo profile" unless tunnel["agent"]["profiles"] == ["origin-demo"]

# A failing `up` must still dump logs + tear down, in a project that cannot clobber the demo.
plan = ENV["SMOKE_PLAN"]
errs << "deploy-smoke must use its own project (-p gsp-smoke)" unless plan.include?("-p gsp-smoke")
errs << "deploy-smoke must dump logs on any failure" unless plan.include?(" logs")
errs << "deploy-smoke runs `up` as its own recipe line: a failed up skips logs + down" if plan.lines.any? { |l| l =~ /\Adocker compose.* up / && !l.include?("|| rc=") }

# gsp image needs a 65532-owned /data (tunnel key file volume).
df = File.read("deploy/Dockerfile")
gsp_stage = df[/AS gsp\n.*?(?=\nFROM |\z)/m]
errs << "gsp image lacks a 65532-owned /data" unless gsp_stage.include?("--chown=65532:65532 /out/data /data")
minimal_stage = df[/AS gsp-minimal\n.*?(?=\nFROM |\z)/m]
errs << "gsp-minimal image lacks a 65532-owned /data" unless minimal_stage.to_s.include?("--chown=65532:65532 /out/data /data")

# Prebuilt mode (CI feeds binaries from the shared release build): the stage
# selector, the prebuilt stage, runtime stages reading from `bins`, and the
# compose/build-images plumbing must all agree.
errs << "Dockerfile lacks `ARG BIN_SOURCE=builder` before the first FROM" unless df =~ /\A(?:#[^\n]*\n|\n)*ARG BIN_SOURCE=builder\n/
errs << "Dockerfile lacks `FROM ${BIN_SOURCE} AS bins`" unless df.include?("FROM ${BIN_SOURCE} AS bins")
errs << "Dockerfile lacks a `prebuilt` stage" unless df =~ /^FROM \S+ AS prebuilt$/
errs << "a runtime stage still copies from builder directly" if df.split("# --- runtime targets").last.include?("--from=builder")
errs << "compose build must pass BIN_SOURCE" unless base["gsp"]["build"]["args"].is_a?(Hash) && base["gsp"]["build"]["args"]["BIN_SOURCE"] == "builder"
errs << "build-images.sh must pass --build-arg BIN_SOURCE" unless File.read("deploy/build-images.sh").include?("--build-arg BIN_SOURCE=")

# The tunnel override must use the address authority, not hand-picked addresses.
ctl_cmd = tunnel["controller"]["command"]
errs << "tunnel override: controller needs --tunnel-network=fd49:89c1:4b5e:60::/64 (IPv6 is the default)" unless ctl_cmd.include?("--tunnel-network=fd49:89c1:4b5e:60::/64")
# Both tunnel services share the host network namespace, where runc refuses
# net.* sysctls ("not allowed in host network namespace"): `up` would fail.
%w[gsp agent].each { |s| errs << "tunnel #{s} sets a net.* sysctl, which host networking refuses" if (tunnel[s]["sysctls"] || {}).keys.any? { |k| k.start_with?("net.") } }
errs << "tunnel override: controller lost the base flags" unless %w[--listen=0.0.0.0:9901 --data-dir=/data].all? { |f| ctl_cmd.include?(f) } && ctl_cmd.any? { |a| a.start_with?("--auth-token=") }
errs << "tunnel override: gsp must not hand-pick --tunnel-address" if tunnel["gsp"]["command"].any? { |a| a.start_with?("--tunnel-address") }
agent_cmd = tunnel["agent"]["command"]
errs << "tunnel override: agent must not hand-pick --address" if agent_cmd.any? { |a| a.start_with?("--address") }
errs << "tunnel override: agent backends should use the :port shorthand" unless agent_cmd.include?("--backends=:25565")

if errs.empty? then puts "deploy lint: ok" else warn errs.map { |e| "LINT FAIL: #{e}" }; exit 1 end
'
