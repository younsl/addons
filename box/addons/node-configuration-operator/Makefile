OPERATOR   := node-configuration-operator
AGENT      := node-configuration-agent
VERSION    := $(shell grep -oE 'org\.opencontainers\.image\.version="[^"]+"' operator/Dockerfile | cut -d'"' -f2)
REGISTRY   ?= ghcr.io/younsl
COVER_MIN  ?= 70
TARGET_amd64 := x86_64-unknown-linux-musl
TARGET_arm64 := aarch64-unknown-linux-musl

.PHONY: all build release crd test coverage fmt lint check clean \
        zigbuild-amd64 zigbuild-arm64 zigbuild docker-build docker-push help

.DEFAULT_GOAL := help

## all: fmt + lint + test + build
all: fmt lint test build

## build: compile debug binaries
build:
	cargo build --workspace

## release: compile optimized release binaries
release:
	cargo build --release --bin $(OPERATOR) --bin $(AGENT)

## crd: regenerate the CRDs in the Helm chart from the Rust types
crd:
	cargo run --quiet --bin crdgen

## test: run the tests of every crate
test:
	cargo test --workspace

## coverage: enforce minimum line coverage of both images (cargo-llvm-cov)
coverage:
	cargo llvm-cov -p $(OPERATOR) --fail-under-lines $(COVER_MIN)
	cargo llvm-cov -p $(AGENT) --fail-under-lines $(COVER_MIN)

## fmt: format code
fmt:
	cargo fmt --all

## lint: rustfmt check + clippy with warnings denied
lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

## check: type-check without building
check:
	cargo check --workspace

## clean: remove build artifacts
clean:
	cargo clean
	rm -f operator/$(OPERATOR)-linux-* agent/$(AGENT)-linux-*

## zigbuild-amd64: cross-compile static linux/amd64 binaries
## zigbuild-arm64: cross-compile static linux/arm64 binaries
zigbuild-amd64 zigbuild-arm64: zigbuild-%:
	cargo zigbuild --release --bin $(OPERATOR) --bin $(AGENT) --target $(TARGET_$*)
	cp target/$(TARGET_$*)/release/$(OPERATOR) operator/$(OPERATOR)-linux-$*
	cp target/$(TARGET_$*)/release/$(AGENT) agent/$(AGENT)-linux-$*

## zigbuild: cross-compile both binaries for both architectures
zigbuild: zigbuild-amd64 zigbuild-arm64

## docker-build: cross-compile, then build both multi-arch images (requires buildx)
docker-build: zigbuild
	docker buildx build --platform linux/amd64,linux/arm64 \
		-t $(REGISTRY)/$(OPERATOR):$(VERSION) -t $(REGISTRY)/$(OPERATOR):latest operator
	docker buildx build --platform linux/amd64,linux/arm64 \
		-t $(REGISTRY)/$(AGENT):$(VERSION) -t $(REGISTRY)/$(AGENT):latest agent

## docker-push: cross-compile, then build and push both multi-arch images
docker-push: zigbuild
	docker buildx build --platform linux/amd64,linux/arm64 \
		-t $(REGISTRY)/$(OPERATOR):$(VERSION) -t $(REGISTRY)/$(OPERATOR):latest --push operator
	docker buildx build --platform linux/amd64,linux/arm64 \
		-t $(REGISTRY)/$(AGENT):$(VERSION) -t $(REGISTRY)/$(AGENT):latest --push agent

## help: show this message
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## //' | column -t -s ':'
