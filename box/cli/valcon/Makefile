.PHONY: build release test fmt lint coverage screenshots install clean help

BINARY_NAME := valcon

.DEFAULT_GOAL := help

## Build debug binary
build:
	cargo build

## Build optimized release binary
release:
	cargo build --release

## Run tests
test:
	cargo test

## Format code
fmt:
	cargo fmt

## Run clippy on all targets
lint:
	cargo clippy --all-targets -- -D warnings

## Line coverage summary (release gate is 70%)
coverage:
	cargo llvm-cov --summary-only

## Render docs/assets/screenshots from fixture data
screenshots:
	cargo test screenshots -- --ignored

## Install to ~/.cargo/bin/
install:
	cargo install --path .

## Clean build artifacts
clean:
	cargo clean

## Show this help message
help:
	@echo "Available targets:"
	@echo ""
	@grep -E '^##' -A1 $(MAKEFILE_LIST) | grep -vE '^--' | paste - - | sed -E 's/^## (.*)\t([a-z-]+):.*/  \2\t\1/' | column -t -s "$$(printf '\t')"
