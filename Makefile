# Convenience wrapper around the cargo commands CI runs.
# Requires `cargo` on PATH (rustup: `source "$HOME/.cargo/env"`).

.PHONY: check fmt lint test test-minimal audit build run fuzz bench plugins ui ui-test ui-e2e tunnel-ns-check tunnel-e2e tunnel-e2e-ci deploy-images deploy-scan deploy-lint deploy-smoke docs-fmt docs-fmt-check docs-links help

## check: everything CI runs — format check, clippy (deny warnings), tests
check: fmt-check lint test test-minimal

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

## test-minimal: the minimal edge build of wayhouse (all optional cargo features off, issue #62)
test-minimal:
	cargo clippy -p wayhouse --no-default-features --all-targets -- -D warnings
	cargo test -p wayhouse --no-default-features
	cargo clippy -p wayhouse-core --no-default-features --all-targets -- -D warnings
	cargo test -p wayhouse-core --no-default-features

## audit: scan dependencies for known vulnerabilities (cargo install cargo-audit --locked)
audit:
	sh .github/scripts/cargo_audit.sh

## build: debug build of the workspace
build:
	cargo build

## run: run the proxy against the example config
run:
	cargo run -p wayhouse -- --config config.example.yaml

## bench: latency / load harness vs. the NFR N1/N2 targets (see crates/wayhouse-bench)
BENCH_ARGS ?=
bench:
	cargo run --release -p wayhouse-bench -- $(BENCH_ARGS)

## fuzz: short pass of each wayhouse-config fuzz target (needs nightly + cargo-fuzz)
FUZZ_TIME ?= 60
fuzz:
	cd crates/wayhouse-config && for t in extract_sni route_match parse_config; do \
		echo "--- fuzz $$t ($(FUZZ_TIME)s) ---"; \
		mkdir -p fuzz/corpus/$$t; \
		cargo +nightly fuzz run $$t fuzz/corpus/$$t fuzz/seeds/$$t -- -max_total_time=$(FUZZ_TIME) || exit 1; \
	done

## plugins: build the first-party sniffer plugins to wasm32-unknown-unknown
## (needs `rustup target add wasm32-unknown-unknown`); see crates/plugins/README.md
plugins:
	cd crates/plugins && cargo test --workspace
	cd crates/plugins && cargo build --release --target wasm32-unknown-unknown -p a2s -p minecraft -p quic -p regex-firstbytes -p wireguard -p openvpn -p raknet -p teamspeak3
	@echo "built:" crates/plugins/target/wasm32-unknown-unknown/release/*.wasm

## ui: build the wayhouse-ui frontend (needs Node/npm) — output wayhouse-ui serves via --static-dir
ui:
	cd crates/wayhouse-ui/web && npm install && npm run build

## ui-test: run the wayhouse-ui frontend tests (vitest + Testing Library; needs Node/npm)
ui-test:
	cd crates/wayhouse-ui/web && npm install && npm test

## ui-e2e: run the wayhouse-ui Playwright browser tests against the built UI (backend stubbed; first run: npx playwright install chromium)
ui-e2e:
	cd crates/wayhouse-ui/web && npm install && npm run test:e2e

# Rootless when not already root: a user+net+mount namespace gives us
# CAP_NET_ADMIN inside it. `--kill-child` reaps everything if we die.
ifeq ($(shell id -u),0)
TUNNEL_NS := unshare -m -n --kill-child
else
TUNNEL_NS := unshare -Urnm --kill-child
endif

# Fails early with the fix when unprivileged user namespaces are blocked (Ubuntu's
# AppArmor), before the build and the in-test hint that only appears once the
# test binary runs inside the namespace.
tunnel-ns-check:
	@$(TUNNEL_NS) true 2>/dev/null || { \
	  echo "cannot create a user+net namespace with: $(TUNNEL_NS) true" >&2; \
	  echo "on Ubuntu 24.04+, AppArmor blocks unprivileged user namespaces; allow them with" >&2; \
	  echo "  sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0" >&2; \
	  echo "or run this target under sudo (the CI job does the former)" >&2; \
	  exit 1; }

## tunnel-e2e: phase-14 WireGuard tunnel end-to-end test in rootless network
## namespaces (TUNNEL_BACKEND=kernel|userspace, default kernel); see
## docs/superpowers/specs/2026-10-01-tunnel-e2e-design.md
tunnel-e2e: tunnel-ns-check
	cargo build -p wayhouse -p wayhouse-agent -p wayhouse-controller
	cargo test -p wayhouse-fleet-tests --test tunnel --no-run
	$(TUNNEL_NS) sh -c 'mount -t tmpfs tmpfs /run && mkdir -p /run/wireguard && exec cargo test -p wayhouse-fleet-tests --test tunnel -- --ignored --test-threads=1 --nocapture'

## tunnel-e2e-ci: tunnel-e2e under cargo-nextest, writing a JUnit report for the CI run
## summary (needs cargo-nextest; `make tunnel-e2e` stays on plain cargo test)
tunnel-e2e-ci: tunnel-ns-check
	cargo build -p wayhouse -p wayhouse-agent -p wayhouse-controller
	cargo nextest run -p wayhouse-fleet-tests --test tunnel --profile ci --run-ignored only --no-run
	$(TUNNEL_NS) sh -c 'mount -t tmpfs tmpfs /run && mkdir -p /run/wireguard && exec cargo nextest run -p wayhouse-fleet-tests --test tunnel --profile ci --run-ignored only -j1 --no-capture'

## deploy-images: build the five deploy/ images and run --version on each (needs Docker)
deploy-images:
	sh deploy/build-images.sh

## deploy-scan: Trivy over the deploy/ images and the lockfiles they're built from (after deploy-images; needs Docker + trivy)
deploy-scan:
	sh deploy/scan-images.sh

## docs-fmt: format Markdown with Prettier (needs Node)
docs-fmt:
	npx --yes prettier@3.9.9 --write "**/*.md"

## docs-fmt-check: verify Markdown formatting (what the `docs` CI job runs)
docs-fmt-check:
	npx --yes prettier@3.9.9 --check "**/*.md"

## docs-links: check that relative Markdown links and anchors resolve
docs-links:
	python3 .github/scripts/check_md_links.py README.md CONTRIBUTING.md AGENTS.md HANDOVER.md RELEASING.md deploy/README.md

## deploy-lint: static checks on deploy/ (needs the docker CLI + ruby, no daemon)
deploy-lint:
	sh deploy/lint.sh

# Own project name so `down -v` can never touch a hand-run demo (wayhouse-demo).
DEPLOY_COMPOSE := docker compose -p wayhouse-smoke --env-file deploy/compose/.env -f deploy/compose/docker-compose.yml

## deploy-smoke: build + start deploy/compose, run deploy/smoke.sh, tear down (needs Docker)
deploy-smoke:
	test -f deploy/compose/.env || cp deploy/compose/.env.example deploy/compose/.env
	rc=0; \
	sha="$${WAYHOUSE_GIT_SHA:-$$(git rev-parse HEAD 2>/dev/null)}"; export WAYHOUSE_GIT_SHA=$$(printf %.12s "$$sha"); \
	$(DEPLOY_COMPOSE) up -d --build && sh deploy/smoke.sh || rc=$$?; \
	[ $$rc -eq 0 ] || $(DEPLOY_COMPOSE) logs; \
	$(DEPLOY_COMPOSE) down -v; \
	exit $$rc

## help: list targets
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## /  /'
