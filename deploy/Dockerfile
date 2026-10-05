# syntax=docker/dockerfile:1
# Where the runtime targets take their binaries from:
#   builder   (default) compile inside this Dockerfile — self-contained.
#   prebuilt  copy binaries compiled outside Docker from deploy/prebuilt/
#             (CI's shared release build; `BIN_SOURCE=prebuilt make deploy-images`).
ARG BIN_SOURCE=builder
# Base images are pinned by digest (the tag stays for readers), so rebuilding a
# release tag gives the same bases. To bump one: resolve the tag's current index
# digest (`docker buildx imagetools inspect <image>:<tag>`), replace it everywhere
# the image appears below, and let CI (build, smoke, Trivy) judge the new base.
# Reference images (docs/12). Build one with:
#   docker build -f deploy/Dockerfile --target wayhouse-controller -t wayhouse-controller .

# --- frontend (only the wayhouse-ui target uses it) ---------------------------
FROM node:22-bookworm-slim@sha256:43ac6c60b8f89723f746e8a92ce91abd5017e627ce1ddfe4238355d3a30b772c AS web
WORKDIR /web
COPY crates/wayhouse-ui/web/package.json crates/wayhouse-ui/web/package-lock.json ./
RUN npm ci
COPY crates/wayhouse-ui/web/ ./
RUN npm run build

# --- one builder for all five binaries (deps compile once) ---------------
# Plus wayhouse-minimal: `wayhouse` built with `--no-default-features` (issue #62), first,
# so the full build below leaves target/release/wayhouse the full one.
FROM rust:1-trixie@sha256:3745c050d12adc738eff16ebfc81ed044bfb2cc27c6828850ff1666beb1c7a49 AS builder
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
# The commit reported by wayhouse_build_info (`crates/wayhouse-http/build.rs`). There is no
# .git in this build (.dockerignore), so the caller passes it: build-images.sh,
# compose and release.yml all do. Empty falls back to commit="unknown".
ARG WAYHOUSE_GIT_SHA=
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    mkdir -p /out/data \
 && cargo auditable build --release --locked -p wayhouse --no-default-features \
 && cp target/release/wayhouse /out/wayhouse-minimal \
 && cargo auditable build --release --locked \
      -p wayhouse -p wayhouse-controller -p wayhouse-aggregator -p wayhouse-ui -p wayhouse-agent \
 && cp target/release/wayhouse target/release/wayhouse-controller \
       target/release/wayhouse-aggregator target/release/wayhouse-ui \
       target/release/wayhouse-agent /out/ \
 && chown 65532:65532 /out/data

# --- prebuilt binaries (BIN_SOURCE=prebuilt) -----------------------------
# Binaries built on the CI runner (glibc <= the runtime's, so they run on it).
FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a AS prebuilt
COPY --chmod=0755 deploy/prebuilt/ /out/
RUN mkdir -p /out/data && chown 65532:65532 /out/data

FROM ${BIN_SOURCE} AS bins

# --- runtime targets ------------------------------------------------------
# distroless/cc-debian13 (glibc 2.41 == the builder's) has CA certs, no shell.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2 AS wayhouse
COPY --from=bins /out/wayhouse /usr/local/bin/wayhouse
# Writable by the nonroot uid: the tunnel key file lives on a volume here.
COPY --from=bins --chown=65532:65532 /out/data /data
ENTRYPOINT ["/usr/local/bin/wayhouse"]
CMD ["--config", "/etc/wayhouse/wayhouse.yaml"]

# The minimal edge build of wayhouse (`--no-default-features`: no WASM sniffers, gRPC
# resolver, dns_srv source, gossip fabric or tunnel client; a config that needs
# one is refused at startup). Installed as `wayhouse`, so it takes the same arguments as the `wayhouse` target.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2 AS wayhouse-minimal
COPY --from=bins /out/wayhouse-minimal /usr/local/bin/wayhouse
COPY --from=bins --chown=65532:65532 /out/data /data
ENTRYPOINT ["/usr/local/bin/wayhouse"]
CMD ["--config", "/etc/wayhouse/wayhouse.yaml"]

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2 AS wayhouse-controller
COPY --from=bins /out/wayhouse-controller /usr/local/bin/wayhouse-controller
# /data must be owned by the nonroot uid *in the image* so a fresh named
# volume / PVC inherits a writable directory.
COPY --from=bins --chown=65532:65532 /out/data /data
ENTRYPOINT ["/usr/local/bin/wayhouse-controller"]
CMD ["--listen", "0.0.0.0:9901", "--data-dir", "/data"]

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2 AS wayhouse-aggregator
COPY --from=bins /out/wayhouse-aggregator /usr/local/bin/wayhouse-aggregator
ENTRYPOINT ["/usr/local/bin/wayhouse-aggregator"]
CMD ["--listen", "0.0.0.0:9902"]

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2 AS wayhouse-ui
COPY --from=bins /out/wayhouse-ui /usr/local/bin/wayhouse-ui
COPY --from=web /web/dist /web
ENTRYPOINT ["/usr/local/bin/wayhouse-ui", "--static-dir", "/web"]
CMD ["--listen", "0.0.0.0:9903"]

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2 AS wayhouse-agent
COPY --from=bins /out/wayhouse-agent /usr/local/bin/wayhouse-agent
COPY --from=bins --chown=65532:65532 /out/data /data
ENTRYPOINT ["/usr/local/bin/wayhouse-agent"]
CMD ["--data-dir", "/data"]
