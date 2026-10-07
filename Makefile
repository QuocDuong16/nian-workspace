.DEFAULT_GOAL := help

CARGO ?= cargo
MISE ?= mise
RUSTUP ?= rustup

CROSS_TARGETS ?= \
	x86_64-pc-windows-msvc \
	x86_64-apple-darwin \
	aarch64-apple-darwin \
	aarch64-unknown-linux-gnu

.PHONY: help toolchain-install format format-check clippy test audit outdated build quality-check check \
	cross-check ci-check run install clean

##@ Getting started
help: ## Show the documented Make targets
	@awk 'BEGIN { FS = ":.*##"; printf "Usage: make <target> [VAR=value...]\n" } /^##@/ { if (shown++) printf "\n"; printf "%s\n", substr($$0, 5); next } /^[a-zA-Z0-9_.-]+:.*##/ { printf "  \033[36m%-32s\033[0m %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

##@ Toolchain
toolchain-install: ## Install Rust and Cargo tools pinned by mise
	$(MISE) install --locked rust cargo:cargo-audit cargo:cargo-outdated

##@ Rust workspace
format: ## Format Rust sources
	$(CARGO) fmt --all

format-check: ## Check Rust formatting without changing files
	$(CARGO) fmt --all --check

clippy: ## Run Clippy with warnings denied
	$(CARGO) clippy --all-targets --all-features -- -D warnings

test: ## Run the full test suite
	$(CARGO) test

audit: ## Scan locked dependencies against the RustSec advisory database
	$(MISE) exec -- cargo audit

outdated: ## List newer versions of direct dependencies
	$(MISE) exec -- cargo outdated --root-deps-only

build: ## Build the optimized release binary
	$(CARGO) build --release

##@ Quality and cross-target checks
quality-check: format-check clippy test build ## Run the native quality gates used by CI

check: quality-check ## Alias for quality-check

cross-check: ## Compile-check the CI targets (installs their Rust standard libraries)
	$(RUSTUP) target add $(CROSS_TARGETS)
	@set -eu; \
	for target in $(CROSS_TARGETS); do \
		echo "==> cargo check --all-targets --all-features --target $$target"; \
		$(CARGO) check --all-targets --all-features --target "$$target"; \
	done

ci-check: quality-check cross-check ## Run native and cross-target CI checks locally

##@ CLI and installation
run: ## Run the CLI (pass arguments with ARGS="...")
	$(CARGO) run -- $(ARGS)

install: ## Install the binary from this checkout
	$(CARGO) install --path . --locked

##@ Cleanup
clean: ## Remove Cargo build artifacts
	$(CARGO) clean
