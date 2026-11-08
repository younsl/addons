BINARY     := ward
VERSION    := $(shell grep -oE 'org\.opencontainers\.image\.version="[^"]+"' Dockerfile | cut -d'"' -f2)
REGISTRY   ?= ghcr.io/younsl
IMAGE      := $(REGISTRY)/$(BINARY)
COVER_MIN  ?= 70

BPF_LINKER_VERSION ?= 0.13.0
# Digests of the v0.13.0 release assets, checked before the binary runs
BPF_LINKER_SHA256_x86_64-unknown-linux-musl := 0e9b2b30f68898177f1cb6f0ecbfa909a82479473a49a0ef66529fd088e3e410
BPF_LINKER_SHA256_aarch64-unknown-linux-musl := 248e25f5c6a7242b054bd8f705470abb7a5a79b2b7c07df347c5d7049b783452
BPF_LINKER_SHA256_x86_64-apple-darwin := 4f33e0aac922606522fe5e4a3a23a5ed8083c3af50a05108301315e50b995502
BPF_LINKER_SHA256_aarch64-apple-darwin := a4882533f5d423f8397ba431256d1cef45a3e13dbfc359decafee3800ee90e55
TOOLS_DIR   := $(CURDIR)/.tools
BPF_LINKER  := $(TOOLS_DIR)/bpf-linker
EBPF_ARCHES := x86_64 aarch64

HOST_OS   := $(shell uname -s)
HOST_ARCH := $(shell uname -m | sed 's/arm64/aarch64/')
ifeq ($(HOST_OS),Darwin)
HOST_TRIPLE := $(HOST_ARCH)-apple-darwin
else
HOST_TRIPLE := $(HOST_ARCH)-unknown-linux-musl
endif

.PHONY: all build release run crd test coverage fmt lint check clean tools ebpf \
        zigbuild-amd64 zigbuild-arm64 zigbuild docker-build docker-push help

.DEFAULT_GOAL := help

## all: fmt + lint + test + build
all: fmt lint test build

## build: compile debug binary (embeds the eBPF object when `make ebpf` ran)
build:
	cargo build

## release: compile optimized release binary
release:
	cargo build --release

## run: run the operator against the current kubeconfig (WARD_IMAGE=ref to override)
run:
	WARD_IMAGE=$${WARD_IMAGE:-$(IMAGE):$(VERSION)} cargo run -- --log-format text operator

## crd: regenerate the chart CRD from the Rust types
crd:
	cargo run --quiet --bin crdgen

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

## check: type-check without building
check:
	cargo check

## clean: remove build artifacts
clean:
	cargo clean
	cargo clean --manifest-path ebpf/Cargo.toml
	rm -f $(BINARY)-linux-amd64 $(BINARY)-linux-arm64

## tools: download the prebuilt bpf-linker for this host into .tools/
tools: $(BPF_LINKER)

$(BPF_LINKER):
	mkdir -p $(TOOLS_DIR)
	curl -fsSL -o $(TOOLS_DIR)/bpf-linker.tar.zst \
		https://github.com/aya-rs/bpf-linker/releases/download/v$(BPF_LINKER_VERSION)/bpf-linker-$(HOST_TRIPLE).tar.zst
	echo "$(BPF_LINKER_SHA256_$(HOST_TRIPLE))  $(TOOLS_DIR)/bpf-linker.tar.zst" | shasum -a 256 -c -
	tar --zstd -xf $(TOOLS_DIR)/bpf-linker.tar.zst -C $(TOOLS_DIR)
	rm -f $(TOOLS_DIR)/bpf-linker.tar.zst
	chmod +x $(BPF_LINKER)

## ebpf: build the kernel programs for both arches (pinned stable toolchain, build-std via RUSTC_BOOTSTRAP)
ebpf: tools
	rustup component add rust-src
	@for arch in $(EBPF_ARCHES); do \
		echo "building eBPF object for $$arch"; \
		(cd ebpf && PATH="$(TOOLS_DIR):$$PATH" RUSTC_BOOTSTRAP=1 \
			CARGO_ENCODED_RUSTFLAGS="$$(printf -- '--cfg=bpf_target_arch="%s"\037-Cdebuginfo=2\037-Clink-arg=--btf' $$arch)" \
			cargo build --release -Z build-std=core --target bpfel-unknown-none --target-dir target/$$arch) || exit 1; \
	done

## zigbuild-amd64: cross-compile static linux/amd64 binary with the eBPF object
zigbuild-amd64: ebpf
	cargo zigbuild --release --target x86_64-unknown-linux-musl --bin $(BINARY)
	cp target/x86_64-unknown-linux-musl/release/$(BINARY) $(BINARY)-linux-amd64

## zigbuild-arm64: cross-compile static linux/arm64 binary with the eBPF object
zigbuild-arm64: ebpf
	cargo zigbuild --release --target aarch64-unknown-linux-musl --bin $(BINARY)
	cp target/aarch64-unknown-linux-musl/release/$(BINARY) $(BINARY)-linux-arm64

## zigbuild: cross-compile both architectures
zigbuild: zigbuild-amd64 zigbuild-arm64

## docker-build: cross-compile both arches, then build multi-arch image (requires buildx)
docker-build: zigbuild
	docker buildx build --platform linux/amd64,linux/arm64 \
		-t $(IMAGE):$(VERSION) -t $(IMAGE):latest .

## docker-push: cross-compile both arches, then build and push multi-arch image
docker-push: zigbuild
	docker buildx build --platform linux/amd64,linux/arm64 \
		-t $(IMAGE):$(VERSION) -t $(IMAGE):latest --push .

## help: show this message
help:
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## //' | column -t -s ':'
