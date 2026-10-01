# syntax=docker/dockerfile:1
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
FROM rust:1-trixie AS builder
# rust-toolchain.toml is deliberately not copied: with it, rustup would fetch
# rustfmt/clippy during the build.
ENV RUSTUP_TOOLCHAIN=stable
RUN apt-get update \
 && apt-get install -y --no-install-recommends protobuf-compiler \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
      -p gsp -p gsp-controller -p gsp-aggregator -p gsp-ui -p gsp-agent \
 && mkdir -p /out/data \
 && cp target/release/gsp target/release/gsp-controller \
       target/release/gsp-aggregator target/release/gsp-ui \
       target/release/gsp-agent /out/ \
 && chown 65532:65532 /out/data

# --- runtime targets ------------------------------------------------------
# distroless/cc-debian13 (glibc 2.41 == the builder's) has CA certs, no shell.
FROM gcr.io/distroless/cc-debian13:nonroot AS gsp
COPY --from=builder /out/gsp /usr/local/bin/gsp
# Writable by the nonroot uid: the tunnel key file lives on a volume here.
COPY --from=builder --chown=65532:65532 /out/data /data
ENTRYPOINT ["/usr/local/bin/gsp"]
CMD ["--config", "/etc/gsp/gsp.yaml"]

FROM gcr.io/distroless/cc-debian13:nonroot AS gsp-controller
COPY --from=builder /out/gsp-controller /usr/local/bin/gsp-controller
# /data must be owned by the nonroot uid *in the image* so a fresh named
# volume / PVC inherits a writable directory.
COPY --from=builder --chown=65532:65532 /out/data /data
ENTRYPOINT ["/usr/local/bin/gsp-controller"]
CMD ["--listen", "0.0.0.0:9901", "--data-dir", "/data"]

FROM gcr.io/distroless/cc-debian13:nonroot AS gsp-aggregator
COPY --from=builder /out/gsp-aggregator /usr/local/bin/gsp-aggregator
ENTRYPOINT ["/usr/local/bin/gsp-aggregator"]
CMD ["--listen", "0.0.0.0:9902"]

FROM gcr.io/distroless/cc-debian13:nonroot AS gsp-ui
COPY --from=builder /out/gsp-ui /usr/local/bin/gsp-ui
COPY --from=web /web/dist /web
ENTRYPOINT ["/usr/local/bin/gsp-ui", "--static-dir", "/web"]
CMD ["--listen", "0.0.0.0:9903"]

FROM gcr.io/distroless/cc-debian13:nonroot AS gsp-agent
COPY --from=builder /out/gsp-agent /usr/local/bin/gsp-agent
COPY --from=builder --chown=65532:65532 /out/data /data
ENTRYPOINT ["/usr/local/bin/gsp-agent"]
CMD ["--data-dir", "/data"]
