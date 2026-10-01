# Convenience wrapper around the cargo commands CI runs.
# Requires `cargo` on PATH (rustup: `source "$HOME/.cargo/env"`).

.PHONY: check fmt lint test audit build run fuzz bench plugins ui ui-test tunnel-e2e tunnel-e2e-ci deploy-images deploy-lint deploy-smoke help

## check: everything CI runs — format check, clippy (deny warnings), tests
check: fmt-check lint test

## fmt: apply rustfmt
fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all --check

## lint: clippy with warnings denied
lint:
	cargo clippy --all-targets --all-features -- -D warnings

## test: run the whole test suite
test:
	cargo test --all

## audit: scan dependencies for known vulnerabilities (cargo install cargo-audit --locked)
audit:
	cargo audit

## build: debug build of the workspace
build:
	cargo build

## run: run the proxy against the example config
run:
	cargo run -p gsp -- --config config.example.yaml

## bench: latency / load harness vs. the NFR N1/N2 targets (see crates/gsp-bench)
BENCH_ARGS ?=
bench:
	cargo run --release -p gsp-bench -- $(BENCH_ARGS)

## fuzz: short pass of each gsp-config fuzz target (needs nightly + cargo-fuzz)
FUZZ_TIME ?= 60
fuzz:
	cd crates/gsp-config && for t in extract_sni route_match parse_config; do \
		echo "--- fuzz $$t ($(FUZZ_TIME)s) ---"; \
		mkdir -p fuzz/corpus/$$t; \
		cargo +nightly fuzz run $$t fuzz/corpus/$$t fuzz/seeds/$$t -- -max_total_time=$(FUZZ_TIME) || exit 1; \
	done

## plugins: build the first-party sniffer plugins to wasm32-unknown-unknown
## (needs `rustup target add wasm32-unknown-unknown`); see crates/plugins/README.md
plugins:
	cd crates/plugins && cargo test --workspace
	cd crates/plugins && cargo build --release --target wasm32-unknown-unknown -p a2s -p minecraft -p regex-firstbytes
	@echo "built:" crates/plugins/target/wasm32-unknown-unknown/release/*.wasm

## ui: build the gsp-ui frontend (needs Node/npm) — output gsp-ui serves via --static-dir
ui:
	cd crates/gsp-ui/web && npm install && npm run build

## ui-test: run the gsp-ui frontend tests (vitest + Testing Library; needs Node/npm)
ui-test:
	cd crates/gsp-ui/web && npm install && npm test

# Rootless when not already root: a user+net+mount namespace gives us
# CAP_NET_ADMIN inside it. `--kill-child` reaps everything if we die.
ifeq ($(shell id -u),0)
TUNNEL_NS := unshare -m -n --kill-child
else
TUNNEL_NS := unshare -Urnm --kill-child
endif

## tunnel-e2e: phase-14 WireGuard tunnel end-to-end test in rootless network
## namespaces (TUNNEL_BACKEND=kernel|userspace, default kernel); see
## docs/superpowers/specs/2026-10-01-tunnel-e2e-design.md
tunnel-e2e:
	cargo build -p gsp -p gsp-agent -p gsp-controller -p gsp-aggregator -p gsp-ui
	cargo test -p gsp-fleet-tests --test tunnel --no-run
	$(TUNNEL_NS) sh -c 'mount -t tmpfs tmpfs /run && mkdir -p /run/wireguard && exec cargo test -p gsp-fleet-tests --test tunnel -- --ignored --skip known_bug_ --test-threads=1 --nocapture'

## tunnel-e2e-ci: tunnel-e2e under cargo-nextest, writing a JUnit report for the CI run
## summary (needs cargo-nextest; `make tunnel-e2e` stays on plain cargo test)
tunnel-e2e-ci:
	cargo build -p gsp -p gsp-agent -p gsp-controller -p gsp-aggregator -p gsp-ui
	cargo nextest run -p gsp-fleet-tests --test tunnel --profile ci --run-ignored only -E 'not test(known_bug_)' --no-run
	$(TUNNEL_NS) sh -c 'mount -t tmpfs tmpfs /run && mkdir -p /run/wireguard && exec cargo nextest run -p gsp-fleet-tests --test tunnel --profile ci --run-ignored only -E "not test(known_bug_)" -j1 --no-capture'

## deploy-images: build the five deploy/ images and run --version on each (needs Docker)
deploy-images:
	sh deploy/build-images.sh

## deploy-lint: static checks on deploy/ (needs the docker CLI + ruby, no daemon)
deploy-lint:
	sh deploy/lint.sh

# Own project name so `down -v` can never touch a hand-run demo (gsp-demo).
DEPLOY_COMPOSE := docker compose -p gsp-smoke --env-file deploy/compose/.env -f deploy/compose/docker-compose.yml

## deploy-smoke: build + start deploy/compose, run deploy/smoke.sh, tear down (needs Docker)
deploy-smoke:
	test -f deploy/compose/.env || cp deploy/compose/.env.example deploy/compose/.env
	rc=0; \
	$(DEPLOY_COMPOSE) up -d --build && sh deploy/smoke.sh || rc=$$?; \
	[ $$rc -eq 0 ] || $(DEPLOY_COMPOSE) logs; \
	$(DEPLOY_COMPOSE) down -v; \
	exit $$rc

## help: list targets
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## /  /'
