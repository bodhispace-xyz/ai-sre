# Local developer commands. `make ci` intentionally mirrors .github/workflows/ci.yml.

SHELL := /bin/sh

CARGO ?= cargo
RUSTDOCFLAGS ?= -D warnings

# Keep local tool versions aligned with the pinned GitHub Actions toolchain.
NEXTEST_VERSION ?= 0.9.143
CARGO_DENY_VERSION ?= 0.20.2

.DEFAULT_GOAL := help

.PHONY: help run build fmt fmt-check lint test unit-test doc-tests docs deny check-tools ci install-ci-tools

help: ## Show available local commands.
	@awk 'BEGIN {FS = ":.*## "}; /^[a-zA-Z0-9_.-]+:.*## / {printf "\033[36m%-18s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

run: ## Run the binary; pass arguments with ARGS="...".
	$(CARGO) run -- $(ARGS)

build: ## Build the application with all features.
	$(CARGO) build --all-features

fmt: ## Format all Rust source files.
	$(CARGO) fmt --all

fmt-check: ## Verify Rust formatting without changing files.
	$(CARGO) fmt --all -- --check

lint: ## Run strict Clippy across all targets and features.
	$(CARGO) clippy --all-targets --all-features -- -D warnings

test: check-tools ## Run the same nextest profile used by CI.
	$(CARGO) nextest run --all-targets --all-features --profile ci

unit-test: ## Run tests with Cargo's built-in test runner.
	$(CARGO) test --all-features

doc-tests: ## Run documentation tests across all features.
	$(CARGO) test --doc --all-features

docs: ## Build documentation and treat rustdoc warnings as errors.
	RUSTDOCFLAGS="$(RUSTDOCFLAGS)" $(CARGO) doc --no-deps --all-features

deny: check-tools ## Check advisories, bans, licenses, and dependency sources.
	$(CARGO) deny check

check-tools: ## Verify the pinned CI cargo subcommands are installed.
	@command -v cargo-nextest >/dev/null 2>&1 || { \
		echo "cargo-nextest is required; run 'make install-ci-tools'" >&2; \
		exit 1; \
	}
	@command -v cargo-deny >/dev/null 2>&1 || { \
		echo "cargo-deny is required; run 'make install-ci-tools'" >&2; \
		exit 1; \
	}

ci: fmt-check lint test doc-tests docs deny ## Run the complete local CI validation.

install-ci-tools: ## Install the pinned nextest and cargo-deny versions.
	$(CARGO) install --locked cargo-nextest --version $(NEXTEST_VERSION)
	$(CARGO) install --locked cargo-deny --version $(CARGO_DENY_VERSION)
