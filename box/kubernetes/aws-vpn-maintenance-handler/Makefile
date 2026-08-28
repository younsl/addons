.PHONY: build release run dev validate status test coverage fmt lint check clean deps \
        zigbuild-amd64 zigbuild-arm64 zigbuild docker-build docker-buildx \
        helm-lint helm-template docs help

BINARY_NAME := aws-vpn-maintenance-handler
IMAGE_NAME  ?= ghcr.io/younsl/aws-vpn-maintenance-handler
IMAGE_TAG   ?= $(shell grep -oE 'org\.opencontainers\.image\.version="[^"]+"' Dockerfile | cut -d'"' -f2)
COVER_MIN   ?= 70
CONFIG      ?= config.example.yaml
CHART       := charts/$(BINARY_NAME)

RUSTUP := $(shell command -v rustup 2>/dev/null || echo "/opt/homebrew/bin/rustup")
TOOLCHAIN_PATH := $(shell $(RUSTUP) which cargo 2>/dev/null | xargs dirname 2>/dev/null)

# The Slack tokens and Pod identity are required config, so a local run needs stand-ins.
# validate and status never make a Slack call, so placeholders are enough there; export
# real tokens to run the daemon, and they win over these defaults.
LOCAL_ENV = CONFIG_FILE=$(CONFIG) \
	POD_NAMESPACE=$${POD_NAMESPACE:-kube-system} POD_NAME=$${POD_NAME:-local} \
	SLACK_BOT_TOKEN=$${SLACK_BOT_TOKEN:-xoxb-placeholder} \
	SLACK_APP_TOKEN=$${SLACK_APP_TOKEN:-xapp-placeholder}

.DEFAULT_GOAL := help

## Build debug binary
build:
	@PATH="$(TOOLCHAIN_PATH):$$PATH" cargo build

## Build optimized release binary
release:
	@PATH="$(TOOLCHAIN_PATH):$$PATH" cargo build --release

## Run the daemon with the example config
run:
	@$(LOCAL_ENV) LOG_FORMAT=text PATH="$(TOOLCHAIN_PATH):$$PATH" cargo run

## Run the daemon with debug logging and the example config
dev:
	@$(LOCAL_ENV) LOG_FORMAT=text LOG_LEVEL=debug PATH="$(TOOLCHAIN_PATH):$$PATH" cargo run

## Validate the config file without touching AWS or Kubernetes
validate:
	@$(LOCAL_ENV) PATH="$(TOOLCHAIN_PATH):$$PATH" cargo run -q -- validate

## Print tunnel telemetry and pending maintenance, read-only (needs AWS credentials)
status:
	@$(LOCAL_ENV) PATH="$(TOOLCHAIN_PATH):$$PATH" cargo run -q -- status

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
	@PATH="$(TOOLCHAIN_PATH):$$PATH" cargo clippy -- -D warnings

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

## Lint the Helm chart
helm-lint:
	@helm lint $(CHART) \
	    --set config.region=$(or $(AWS_REGION),ap-northeast-2) \
	    --set slack.botToken=xoxb-lint --set slack.appToken=xapp-lint

## Render the Helm chart locally
helm-template:
	@helm template $(BINARY_NAME) $(CHART) \
	    --set config.region=$(or $(AWS_REGION),ap-northeast-2) \
	    --set slack.botToken=xoxb-lint --set slack.appToken=xapp-lint

## Regenerate the chart README via helm-docs (never edit it directly)
docs:
	$(MAKE) -C ../charts docs

## Show this help message
help:
	@echo "Available targets:"
	@echo ""
	@grep -E '^##' $(MAKEFILE_LIST) | sed 's/## /  /' | column -t -s ':'
