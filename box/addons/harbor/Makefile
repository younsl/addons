BINARY     := harbor-arm64
REGISTRY   ?= ghcr.io/younsl/harbor
COVER_MIN  ?= 70

.PHONY: build release test coverage fmt lint dry-run images values push clean help

.DEFAULT_GOAL := help

## build: compile debug binary
build:
	cargo build

## release: compile optimized release binary
release:
	cargo build --release

## test: run tests
test:
	cargo test

## coverage: enforce minimum line coverage (cargo-llvm-cov)
coverage:
	cargo llvm-cov --fail-under-lines $(COVER_MIN)

## fmt: format code
fmt:
	cargo fmt

## lint: rustfmt check + clippy with warnings denied
lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

## dry-run: log every build command without running it
dry-run:
	cargo run -- build --dry-run --push --registry $(REGISTRY)

## images: list the chart images this tool builds
images:
	@cargo run --quiet -- images

## values: print harbor-helm image overrides for $(REGISTRY)
values:
	@cargo run --quiet -- values --registry $(REGISTRY)

## push: build, verify and push every image (native arm64 Docker host only)
push:
	cargo run --release -- build --push --registry $(REGISTRY)

## clean: remove build artifacts
clean:
	cargo clean

## help: show this message
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## //' | column -t -s ':'
