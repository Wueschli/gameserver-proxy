#!/bin/sh
# Static checks on deploy/ that need Docker's CLI but no daemon (compose config
# only renders). Each check pins a failure that only shows up on a real
# `docker compose up` / cluster — see docs/superpowers/plans/2026-10-01-deploy.md.
set -eu
cd "$(dirname "$0")/.."
export GSP_CONTROLLER_TOKEN=x GSP_AGGREGATOR_TOKEN=x GSP_UI_PASSWORD=x \
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

if errs.empty? then puts "deploy lint: ok" else warn errs.map { |e| "LINT FAIL: #{e}" }; exit 1 end
'
