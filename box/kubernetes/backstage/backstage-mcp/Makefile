.PHONY: build release run dev test coverage fmt lint check clean deps \
        zigbuild-amd64 zigbuild-arm64 zigbuild docker-build docker-buildx \
        helm-lint helm-template list-tools help

BINARY_NAME := backstage-mcp
IMAGE_NAME  ?= ghcr.io/younsl/backstage-mcp
IMAGE_TAG   ?= $(shell grep -oE 'org\.opencontainers\.image\.version="[^"]+"' Dockerfile | cut -d'"' -f2)
COVER_MIN   ?= 70

BACKSTAGE_URL ?= http://localhost:7007

RUSTUP := $(shell command -v rustup 2>/dev/null || echo "/opt/homebrew/bin/rustup")
TOOLCHAIN_PATH := $(shell $(RUSTUP) which cargo 2>/dev/null | xargs dirname 2>/dev/null)

.DEFAULT_GOAL := help

## Build debug binary
build:
	@PATH="$(TOOLCHAIN_PATH):$$PATH" cargo build

## Build optimized release binary
release:
	@PATH="$(TOOLCHAIN_PATH):$$PATH" cargo build --release

## Serve over HTTP against a local Backstage backend (reads BACKSTAGE_TOKEN from the environment)
run:
	@BACKSTAGE_URL=$(BACKSTAGE_URL) LOG_FORMAT=text \
	 PATH="$(TOOLCHAIN_PATH):$$PATH" cargo run

## Serve over HTTP with debug logging
dev:
	@BACKSTAGE_URL=$(BACKSTAGE_URL) LOG_FORMAT=text LOG_LEVEL=debug \
	 PATH="$(TOOLCHAIN_PATH):$$PATH" cargo run

## Print the registered tool names
list-tools:
	@BACKSTAGE_URL=$(BACKSTAGE_URL) PATH="$(TOOLCHAIN_PATH):$$PATH" cargo run -q -- --list-tools

## Run tests
test:
	@PATH="$(TOOLCHAIN_PATH):$$PATH" cargo test --verbose

## Enforce minimum line coverage with cargo-llvm-cov
coverage:
	@PATH="$(TOOLCHAIN_PATH):$$PATH" cargo llvm-cov --fail-under-lines $(COVER_MIN)

## Format code
fmt:
	@PATH="$(TOOLCHAIN_PATH):$$PATH" cargo fmt

## Run clippy linter
lint:
	@PATH="$(TOOLCHAIN_PATH):$$PATH" cargo clippy --all-targets -- -D warnings

## Check code without building
check:
	@PATH="$(TOOLCHAIN_PATH):$$PATH" cargo check

## Update dependencies
deps:
	@PATH="$(TOOLCHAIN_PATH):$$PATH" cargo update

## Clean build artifacts
clean:
	@PATH="$(TOOLCHAIN_PATH):$$PATH" cargo clean
	@rm -f $(BINARY_NAME)-linux-*

## Cross-compile to linux/amd64 musl using zig
zigbuild-amd64:
	@PATH="$(TOOLCHAIN_PATH):$$PATH" cargo zigbuild --release --target x86_64-unknown-linux-musl

## Cross-compile to linux/arm64 musl using zig
zigbuild-arm64:
	@PATH="$(TOOLCHAIN_PATH):$$PATH" cargo zigbuild --release --target aarch64-unknown-linux-musl

## Cross-compile to both linux/amd64 and linux/arm64 musl
zigbuild: zigbuild-amd64 zigbuild-arm64

## Build a single-arch image for the host platform (cross-compiles with cargo-zigbuild first)
docker-build:
	@ARCH=$$(docker version -f '{{.Server.Arch}}'); \
	if [ "$$ARCH" = "amd64" ]; then TARGET=x86_64-unknown-linux-musl; else TARGET=aarch64-unknown-linux-musl; fi; \
	PATH="$(TOOLCHAIN_PATH):$$PATH" cargo zigbuild --release --target $$TARGET --bin $(BINARY_NAME); \
	cp target/$$TARGET/release/$(BINARY_NAME) $(BINARY_NAME)-linux-$$ARCH; \
	docker build --platform linux/$$ARCH -t $(IMAGE_NAME):$(IMAGE_TAG) .; \
	rm -f $(BINARY_NAME)-linux-$$ARCH

## Build and push a multi-arch image via buildx
docker-buildx: zigbuild
	@cp target/x86_64-unknown-linux-musl/release/$(BINARY_NAME) $(BINARY_NAME)-linux-amd64; \
	cp target/aarch64-unknown-linux-musl/release/$(BINARY_NAME) $(BINARY_NAME)-linux-arm64; \
	docker buildx build --platform linux/amd64,linux/arm64 \
	    -t $(IMAGE_NAME):$(IMAGE_TAG) -t $(IMAGE_NAME):latest --push .; \
	rm -f $(BINARY_NAME)-linux-amd64 $(BINARY_NAME)-linux-arm64

## Lint Helm chart
helm-lint:
	@helm lint charts/backstage-mcp/

## Render Helm templates locally
helm-template:
	@helm template backstage-mcp charts/backstage-mcp/ --set backstage.token=x --set backstage.url=http://backstage:7007

## Show this help message
help:
	@echo "Available targets:"
	@echo ""
	@grep -E '^##' $(MAKEFILE_LIST) | sed 's/## /  /' | column -t -s ':'
