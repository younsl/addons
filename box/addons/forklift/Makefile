BINARY      := forklift
VERSION     := $(shell grep -oE 'org\.opencontainers\.image\.version="[^"]+"' Dockerfile | cut -d'"' -f2)
MCP_VERSION := $(shell grep -oE 'org\.opencontainers\.image\.version="[^"]+"' Dockerfile.mcp | cut -d'"' -f2)
COMMIT      := $(shell git rev-parse --short HEAD 2>/dev/null || echo none)
ECR_REGISTRY ?= ghcr.io/younsl
IMAGE        := $(ECR_REGISTRY)/$(BINARY)
COVER_MIN   ?= 73
PLATFORMS   ?= linux/amd64,linux/arm64
DATA_DIR    ?= ./.data
# Build metadata is injected at compile time (see src/version.rs).
BUILD_ENV   := FORKLIFT_VERSION=$(VERSION) FORKLIFT_COMMIT=$(COMMIT)
# Cross targets built by `make cross`; cargo-zigbuild links them with zig.
CROSS_TARGETS := x86_64-unknown-linux-musl aarch64-unknown-linux-musl

.PHONY: all build run dev test coverage fmt lint clippy clean cross web-dev web-build docker-context docker-build docker-build-mcp docker-push docker-push-mcp helm-lint helm-template helm-package helm-push creds e2e

all: fmt lint test build

## build: compile the release binaries into bin/
build:
	$(BUILD_ENV) cargo build --release --locked
	@mkdir -p bin && cp target/release/$(BINARY) target/release/$(BINARY)-mcp bin/

## cross: cross-compile static musl binaries for every platform into bin/<target>/
cross:
	@for t in $(CROSS_TARGETS); do \
		$(BUILD_ENV) cargo zigbuild --release --locked --target $$t || exit 1; \
		mkdir -p bin/$$t && cp target/$$t/release/$(BINARY) target/$$t/release/$(BINARY)-mcp bin/$$t/; \
	done

## run: build and run with local dev settings
run: build
	FORKLIFT_DATA_DIR=./.data FORKLIFT_LOG_FORMAT=text ./bin/$(BINARY)

## dev: run with debug logging
dev:
	FORKLIFT_DATA_DIR=./.data FORKLIFT_LOG_FORMAT=text FORKLIFT_LOG_LEVEL=debug cargo run --bin $(BINARY)

## web-dev: run the React UI dev server
web-dev:
	cd web && mise exec -- pnpm dev

## test: run the test suite
test:
	cargo test --all-targets --locked

## coverage: enforce minimum line coverage (requires cargo-llvm-cov)
coverage:
	@total=$$(cargo llvm-cov --all-targets --locked --summary-only --json | \
		python3 -c 'import json,sys; print(json.load(sys.stdin)["data"][0]["totals"]["lines"]["percent"])'); \
	echo "total coverage: $$total% (min $(COVER_MIN)%)"; \
	awk "BEGIN { exit !($$total >= $(COVER_MIN)) }" || { echo "coverage below $(COVER_MIN)%"; exit 1; }

## fmt: format code
fmt:
	cargo fmt --all

## lint: formatting check + clippy with warnings denied
lint: clippy
	cargo fmt --all -- --check

## clippy: run clippy on every target
clippy:
	cargo clippy --all-targets --locked -- -D warnings

## web-build: build the React UI into src/webui/dist (embedded into the binary)
web-build:
	cd web && mise exec -- pnpm install --frozen-lockfile && mise exec -- pnpm run build

## creds: list local users and password hashes from the local DB (plaintext is bcrypt-hashed and unrecoverable; the generated admin password is only printed once in bootstrap logs)
creds:
	@db="$(DATA_DIR)/forklift.db"; \
	if [ ! -f "$$db" ]; then echo "no database at $$db (run 'make run' first)"; exit 1; fi; \
	command -v sqlite3 >/dev/null || { echo "sqlite3 not installed"; exit 1; }; \
	sqlite3 -header -column "$$db" \
		"SELECT id, username, source, disabled, password_hash FROM users ORDER BY id;"

## e2e: run browser tests against a throwaway server and database
# .e2e-data is deleted first on purpose: the server only bootstraps the admin
# account when no user exists, so a leftover database leaves nothing to sign in
# with. `clean` deliberately does not touch it - a run in progress would break.
e2e:
	rm -rf .e2e-data
	cd web && mise exec -- pnpm test:e2e

## clean: remove build and coverage artifacts
clean:
	rm -rf bin target .data $(BINARY)-linux-* $(BINARY)-mcp-linux-*

## docker-context: stage the cross-compiled binaries under the names the Dockerfiles copy
docker-context: web-build cross
	cp bin/x86_64-unknown-linux-musl/$(BINARY) $(BINARY)-linux-amd64
	cp bin/aarch64-unknown-linux-musl/$(BINARY) $(BINARY)-linux-arm64
	cp bin/x86_64-unknown-linux-musl/$(BINARY)-mcp $(BINARY)-mcp-linux-amd64
	cp bin/aarch64-unknown-linux-musl/$(BINARY)-mcp $(BINARY)-mcp-linux-arm64

## docker-build: build the multi-arch forklift image (requires buildx)
docker-build: docker-context
	docker buildx build --platform $(PLATFORMS) \
		-t $(IMAGE):$(VERSION) -t $(IMAGE):latest .

## docker-build-mcp: build the multi-arch forklift-mcp image (requires buildx)
docker-build-mcp: docker-context
	docker buildx build --platform $(PLATFORMS) -f Dockerfile.mcp \
		-t $(IMAGE)-mcp:$(MCP_VERSION) -t $(IMAGE)-mcp:latest .

## docker-push: build and push the multi-arch forklift image
docker-push: docker-context
	docker buildx build --push --platform $(PLATFORMS) \
		-t $(IMAGE):$(VERSION) -t $(IMAGE):latest .

## docker-push-mcp: build and push the multi-arch forklift-mcp image
docker-push-mcp: docker-context
	docker buildx build --push --platform $(PLATFORMS) -f Dockerfile.mcp \
		-t $(IMAGE)-mcp:$(MCP_VERSION) -t $(IMAGE)-mcp:latest .

## helm-lint: lint the Helm chart
helm-lint:
	helm lint charts/$(BINARY)

## helm-template: render Helm templates locally
helm-template:
	helm template $(BINARY) charts/$(BINARY)

## helm-package: package the chart as a tgz
helm-package:
	helm package charts/$(BINARY)

## helm-push: push the packaged chart to the OCI registry (GHCR)
helm-push: helm-package
	helm push $(BINARY)-$(shell grep '^version:' charts/$(BINARY)/Chart.yaml | awk '{print $$2}').tgz oci://$(ECR_REGISTRY)/charts
