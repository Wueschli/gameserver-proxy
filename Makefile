# Convenience wrapper around the cargo commands CI runs.
# Requires `cargo` on PATH (rustup: `source "$HOME/.cargo/env"`).

.PHONY: check fmt lint test build run help

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

## help: list targets
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## /  /'
