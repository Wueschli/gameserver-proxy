# `deploy/` — container images, compose and Kubernetes examples

Date: 2026-10-01 · Status: draft for review · Roadmap item 1 of 3 (then docs/12 TLS
section, then tunnel address authority).

## Intent

`docs/12` describes container images and sizes, but nothing in the repo builds them.
Add reference Dockerfiles and a compose + plain-Kubernetes example, and smoke-test
them in CI so the docs' claims stay true.

**Reference only.** Images are built and tested in CI, never published. The
Dockerfile is written so a later GHCR release workflow is a small follow-up (tags,
multi-arch), but that is out of scope here.

Success = `make deploy-smoke` and the CI `deploy` job build all five images, bring up
the control-plane demo, and pass an HTTP smoke script; the k8s manifests validate
against the Kubernetes schema.

## Non-goals

HA (3-node Raft controller), TLS / Ingress (roadmap item 2), slave tiers, Helm /
kustomize, publishing images, smoke-testing the WireGuard tunnel in containers (that
stays `make tunnel-e2e`; the compose tunnel override is documented, not tested), and
the multi-proxy AllowedIPs bug (item 3).

## Layout

```
.dockerignore                 target/, node_modules, .git, crates/gsp-ui/web/dist
deploy/Dockerfile             one file, five runtime targets
deploy/README.md              build/run/troubleshooting; points at docs/12
deploy/smoke.sh               the HTTP smoke script (used by CI and `make deploy-smoke`)
deploy/compose/docker-compose.yml
deploy/compose/compose.tunnel.yml   override: gsp-agent + --tunnel-* (not smoke-tested)
deploy/compose/.env.example
deploy/compose/gsp.yaml       gsp config seeded into the controller
deploy/k8s/*.yaml             plain manifests
```

## Images (`deploy/Dockerfile`)

- Shared `builder` stage on `rust:1-trixie` runs `cargo build --release --locked` for
  `gsp gsp-controller gsp-aggregator gsp-ui gsp-agent`, so dependencies compile once.
  `protobuf-compiler` is installed (gRPC resolver codegen). `rust-toolchain.toml` is
  not copied and `RUSTUP_TOOLCHAIN=stable` is set, so rustup does not fetch
  rustfmt/clippy during the build.
- Runtime targets `gsp`, `gsp-controller`, `gsp-aggregator`, `gsp-ui`, `gsp-agent`
  each `FROM gcr.io/distroless/cc-debian13:nonroot` and copy exactly one binary.
  trixie → trixie keeps the builder's glibc (2.41) ≤ the runtime's. The `cc-debian12`
  size table in `docs/12` was measured against an older builder; it gets re-measured.
- `gsp-ui` adds a `node:22` stage building `crates/gsp-ui/web` to `dist/`, copied into
  the image; the entrypoint passes `--static-dir`.
- `[profile.release]` gains `strip = true` (docs/12 already prescribes it).
- Images run as `nonroot`. `gsp --tunnel-*` and `gsp-agent` need `NET_ADMIN` and
  `/dev/net/tun`; the examples grant these explicitly (see `docs/12`).
- Control-plane binaries default to `127.0.0.1`; entrypoints/examples pass
  `--listen 0.0.0.0:<port>`. gsp's admin listener (`settings.admin.listen`) likewise
  defaults to loopback and the example config overrides it.
- No shell or `curl` in distroless: health is probed from outside (k8s `httpGet`,
  smoke-script `curl`), never via an in-container `exec`.

## Compose example

Runnable control-plane demo. Services: `controller`, `aggregator`, `ui`, a one-shot
`seed` (POSTs `gsp.yaml` to the controller's `/config`), and `gsp`, started with
`--controller` and `--aggregator`.

- `gsp` uses host networking (docs/12's default for an edge process); the rest sit on
  a bridge network and only the UI port is published.
- Tokens and the UI password come from `.env` (`.env.example` committed, `.env` git-
  ignored); nothing is hard-coded in the YAML. `docker compose --env-file` is how CI
  supplies test values.
- The controller's `--data-dir` is a named volume.
- `compose.tunnel.yml` adds `gsp-agent` and the `--tunnel-*` flags with
  `cap_add: [NET_ADMIN]` and `devices: [/dev/net/tun]`.

**To verify in the plan (unknown today):** whether `gsp --controller` tolerates an
empty controller at start (decides whether `seed` must gate `gsp` via `depends_on:
service_completed_successfully`, which is the intended wiring regardless).

## Kubernetes manifests

Plain YAML, no Helm/kustomize: Deployment + Service for controller, aggregator and UI
(controller gets a PVC); a `gsp` DaemonSet with `hostNetwork: true` and its config in
a ConfigMap, with a commented reserved-range alternative; a `Secret` file of
clearly-marked placeholders; `httpGet` probes on `/healthz` (controller, aggregator,
gsp admin) and `/ui/session` (UI has no `/healthz`). The tunnel `securityContext` /
tun-device block from docs/12 appears as a commented snippet.

## CI and verification

New `deploy` job in `.github/workflows/ci.yml`:

1. `docker build --target <t>` for all five targets (catches glibc mismatch, missing
   `dist/`; prints image sizes).
2. `docker compose up -d --wait` with a test env file.
3. `deploy/smoke.sh`: controller and aggregator `GET /healthz` → ok; UI
   `GET /ui/session` reachable and login works; the aggregator reports the `gsp`
   instance (proves config pull + state push worked end to end).
4. `docker compose down -v`; on failure, dump `docker compose logs`.
5. `kubeconform` (pinned release) validates `deploy/k8s/` — schema only, no cluster.

`make deploy-smoke` runs steps 1–4 locally. The job starts `continue-on-error: true`
(as the tunnel job did) and is made required after a few green runs.

**Local verification caveat:** the Docker daemon is not running in the development
sandbox, so the first authoritative build may be CI. Iterating on a CI branch needs a
push, which stays the owner's call. If the daemon is started locally, run
`make deploy-smoke` first.

## Docs touched (AGENTS.md "when you touch X")

- `docs/12`: link `deploy/`, correct the base-image row to trixie and re-measure
  sizes, drop the "add strip = true" instruction (now done).
- `docs/08` / `README.md` / `HANDOVER.md`: record the landing; HANDOVER's
  next-steps list drops item 1.
- `AGENTS.md` layout block: add `deploy/` and the `make deploy-smoke` command.
