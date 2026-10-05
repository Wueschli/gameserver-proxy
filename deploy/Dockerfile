# syntax=docker/dockerfile:1
# Where the runtime targets take their binaries from:
#   builder   (default) compile inside this Dockerfile — self-contained.
#   prebuilt  copy binaries compiled outside Docker from deploy/prebuilt/
#             (CI's shared release build; `BIN_SOURCE=prebuilt make deploy-images`).
ARG BIN_SOURCE=builder
# Reference images (docs/12). Build one with:
#   docker build -f deploy/Dockerfile --target gsp-controller -t gsp-controller .

# --- frontend (only the gsp-ui target uses it) ---------------------------
FROM node:22-bookworm-slim AS web
WORKDIR /web
COPY crates/gsp-ui/web/package.json crates/gsp-ui/web/package-lock.json ./
RUN npm ci
COPY crates/gsp-ui/web/ ./
RUN npm run build

# --- one builder for all five binaries (deps compile once) ---------------
# Plus gsp-minimal: `gsp` built with `--no-default-features` (issue #62), first,
# so the full build below leaves target/release/gsp the full one.
FROM rust:1-trixie AS builder
# rust-toolchain.toml is deliberately not copied: with it, rustup would fetch
# rustfmt/clippy during the build.
ENV RUSTUP_TOOLCHAIN=stable
RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler \
 && rm -rf /var/lib/apt/lists/*
# cargo-auditable embeds the exact dependency list in each binary (a few KB), so
# an image scan (Trivy) sees the crates a binary was built from. Same version as
# CI's build-release job.
RUN cargo install cargo-auditable --locked --version 0.7.7
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    mkdir -p /out/data \
 && cargo auditable build --release --locked -p gsp --no-default-features \
 && cp target/release/gsp /out/gsp-minimal \
 && cargo auditable build --release --locked \
      -p gsp -p gsp-controller -p gsp-aggregator -p gsp-ui -p gsp-agent \
 && cp target/release/gsp target/release/gsp-controller \
       target/release/gsp-aggregator target/release/gsp-ui \
       target/release/gsp-agent /out/ \
 && chown 65532:65532 /out/data

# --- prebuilt binaries (BIN_SOURCE=prebuilt) -----------------------------
# Binaries built on the CI runner (glibc <= the runtime's, so they run on it).
FROM debian:trixie-slim AS prebuilt
COPY --chmod=0755 deploy/prebuilt/ /out/
RUN mkdir -p /out/data && chown 65532:65532 /out/data

FROM ${BIN_SOURCE} AS bins

# --- runtime targets ------------------------------------------------------
# distroless/cc-debian13 (glibc 2.41 == the builder's) has CA certs, no shell.
FROM gcr.io/distroless/cc-debian13:nonroot AS gsp
COPY --from=bins /out/gsp /usr/local/bin/gsp
# Writable by the nonroot uid: the tunnel key file lives on a volume here.
COPY --from=bins --chown=65532:65532 /out/data /data
ENTRYPOINT ["/usr/local/bin/gsp"]
CMD ["--config", "/etc/gsp/gsp.yaml"]

# The minimal edge build of gsp (`--no-default-features`: no WASM sniffers, gRPC
# resolver, dns_srv source, gossip fabric or tunnel client; a config that needs
# one is refused at startup). Installed as `gsp`, so it takes the same arguments as the `gsp` target.
FROM gcr.io/distroless/cc-debian13:nonroot AS gsp-minimal
COPY --from=bins /out/gsp-minimal /usr/local/bin/gsp
COPY --from=bins --chown=65532:65532 /out/data /data
ENTRYPOINT ["/usr/local/bin/gsp"]
CMD ["--config", "/etc/gsp/gsp.yaml"]

FROM gcr.io/distroless/cc-debian13:nonroot AS gsp-controller
COPY --from=bins /out/gsp-controller /usr/local/bin/gsp-controller
# /data must be owned by the nonroot uid *in the image* so a fresh named
# volume / PVC inherits a writable directory.
COPY --from=bins --chown=65532:65532 /out/data /data
ENTRYPOINT ["/usr/local/bin/gsp-controller"]
CMD ["--listen", "0.0.0.0:9901", "--data-dir", "/data"]

FROM gcr.io/distroless/cc-debian13:nonroot AS gsp-aggregator
COPY --from=bins /out/gsp-aggregator /usr/local/bin/gsp-aggregator
ENTRYPOINT ["/usr/local/bin/gsp-aggregator"]
CMD ["--listen", "0.0.0.0:9902"]

FROM gcr.io/distroless/cc-debian13:nonroot AS gsp-ui
COPY --from=bins /out/gsp-ui /usr/local/bin/gsp-ui
COPY --from=web /web/dist /web
ENTRYPOINT ["/usr/local/bin/gsp-ui", "--static-dir", "/web"]
CMD ["--listen", "0.0.0.0:9903"]

FROM gcr.io/distroless/cc-debian13:nonroot AS gsp-agent
COPY --from=bins /out/gsp-agent /usr/local/bin/gsp-agent
COPY --from=bins --chown=65532:65532 /out/data /data
ENTRYPOINT ["/usr/local/bin/gsp-agent"]
CMD ["--data-dir", "/data"]
