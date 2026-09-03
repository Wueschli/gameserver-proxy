# Convenience wrapper around the cargo commands CI runs.
# Requires `cargo` on PATH (rustup: `source "$HOME/.cargo/env"`).

.PHONY: check fmt lint test build run fuzz bench help

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

## help: list targets
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## /  /'
