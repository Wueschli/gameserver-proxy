# deploy/

Reference container images plus a compose demo and plain Kubernetes manifests
for the five gsp binaries. **Reference only**: nothing here is published —
CI builds and smoke-tests it so `docs/12` stays true. Design:
`docs/superpowers/specs/2026-10-01-deploy-design.md`; networking and capability
background: [`docs/12-deployment.md`](../docs/12-deployment.md).

## What's here

| Path | Purpose |
|---|---|
| `Dockerfile` | one file, five targets: `gsp`, `gsp-controller`, `gsp-aggregator`, `gsp-ui`, `gsp-agent` |
| `build-images.sh` | builds all five, runs `--version` in each, prints sizes |
| `compose/` | runnable control-plane demo (+ `compose.tunnel.yml` WireGuard override) |
| `smoke.sh` | the HTTP smoke test CI runs against the compose demo |
| `k8s/` | plain manifests (namespace, secrets example, controller, aggregator, ui, gsp DaemonSet) |

## Build

```sh
make deploy-images                      # all five, with a --version check each
make deploy-scan                        # then Trivy over them (needs `trivy` on PATH)
docker build -f deploy/Dockerfile --target gsp-controller -t gsp-controller .   # one
```

Builder is `rust:1-trixie`, runtime is `gcr.io/distroless/cc-debian13:nonroot`
(no shell, runs as uid 65532).

`BIN_SOURCE=prebuilt` (default `builder`) skips the in-Docker compile and copies
binaries you built yourself from `deploy/prebuilt/` (`gsp`, `gsp-controller`,
`gsp-aggregator`, `gsp-ui`, `gsp-agent`). CI uses it to share one release build
with the plugins tests; they must be built against a glibc no newer than the
runtime's (2.41). The nightly CI run uses the default, self-contained path.

## Run the demo

```sh
cd deploy/compose
cp .env.example .env        # then change every value
docker compose up -d --build
```

Open <http://127.0.0.1:9903> (password = `GSP_UI_PASSWORD`). The `seed`
one-shot posts `gsp.yaml` to the controller first; `gsp` then pulls it and
pushes its state to the aggregator. `make deploy-smoke` (repo root) does the
same and runs `smoke.sh` against it.

`gsp` uses host networking (docs/12's default for an edge process), so the
controller and aggregator are also published on the host's loopback.

## Tunnel (phase 14)

`docker compose -f docker-compose.yml -f compose.tunnel.yml up -d` adds the
WireGuard transport. It needs `NET_ADMIN`, `/dev/net/tun`, **root in the container**
(the override sets `user: "0"` — a capability is useless to uid 65532) and a
`GSP_TUNNEL_ENDPOINT` (the public `ip:port` origins dial). The demo `agent` is opt-in
(`--profile origin-demo`; it listens on 51821 so it does not clash with `gsp`'s 51820). Not smoke-tested
in CI; `make tunnel-e2e` covers the logic. The controller allocates tunnel addresses from `--tunnel-network` (`10.60.0.0/16` here), so no `--address` / `--tunnel-address` is given; pin one only to keep a specific address (see docs/11 "Address authority"). See docs/12.

## Kubernetes

Retag/push the images to your registry and edit `image:` first, then:

```sh
kubectl apply -f deploy/k8s/00-namespace.yaml
# copy 10-secrets.example.yaml, change every value, apply it
kubectl apply -f deploy/k8s/20-controller.yaml -f deploy/k8s/30-aggregator.yaml \
  -f deploy/k8s/40-ui.yaml -f deploy/k8s/50-gsp-daemonset.yaml
```

`gsp` runs as a `hostNetwork` DaemonSet. The UI Service is ClusterIP — put an
Ingress/LoadBalancer (with TLS) in front of it.

## Caveats

- Reference only; no published images, no multi-arch.
- Tokens are visible in `docker inspect` / the Pod spec env — fine for a demo;
  use real secret management in production.
- These examples run every service on **plain HTTP**: bearer tokens and
  registrations cross the network in the clear. Every service can serve TLS
  itself (`--tls-cert`/`--tls-key`; `settings.admin.tls` for `gsp`'s admin API) — see
  [TLS for the fleet services](../docs/12-deployment.md#tls-for-the-fleet-services).
- One standalone controller; no HA (see docs/10).
- Aggregator intent fan-out (drain etc.) cannot reach `gsp` from these examples: the
  `admin_url` a `gsp` reports is derived from `settings.admin.listen` and no flag
  overrides it. Fleet *reads* (pools, sessions) work.
- `make deploy-scan` runs Trivy over the five images (OS packages, secrets) and over
  `Cargo.lock` and the UI's `package-lock.json` — a plain release binary carries no
  dependency list, so an image scan alone can't see its Rust crates. It reports
  HIGH/CRITICAL findings that have a fix available, writes JSON + SARIF to
  `target/trivy/`, and exits 1 if it found anything. **Informational:** the `deploy` CI
  job (and the nightly run) shows the results on the run's summary page and as
  warnings, and never fails on them. For crates it sees GHSA advisories only —
  RustSec-only ones need `make audit` (`cargo audit`). Accepted findings go in
  `.trivyignore` (repo root), each with a reason.
- `make deploy-lint` runs daemon-free static checks; `make deploy-smoke` uses its own
  compose project (`gsp-smoke`) so it never tears down a demo you started by hand.

## Troubleshooting

- `failed to connect to the docker API` — the Docker daemon isn't running.
- `gsp` exits immediately with "has any config ever been submitted" — the
  controller is empty; the `seed` service must have succeeded.
- `GLIBC_… not found` at container start — builder and runtime base are on
  different Debian releases; keep them on the same one.
- `required variable … is missing` — `.env` is missing or a value is empty.
