# ── Per-backend target directories ───────────────────────────────────────────
# The `zakura` and `lrz` features are mutually exclusive and pull two entirely
# separate crypto stacks, so sharing one target directory makes every backend
# switch invalidate the other's artifacts. Giving each feature permutation its
# own CARGO_TARGET_DIR keeps both warm.
#
# `test-fixtures` is a third feature axis. Every target below that builds the
# default backend enables it, so `check` and `test` resolve to identical
# features and reuse each other's fingerprints.
ZAKURA_TARGET_DIR := $(ROOT)/target/zakura
LRZ_TARGET_DIR    := $(ROOT)/target/lrz
VCT_TARGET_DIR    := $(ROOT)/target/vct

APP_PACKAGES  := -p zcash_voting -p zcash-voting-wallet-example
VCT_PACKAGES  := -p vote-commitment-tree -p vote-commitment-tree-client

# Profile from .config/nextest.toml. `agent` reports failures only; `ci`
# runs the whole suite without failing fast.
NEXTEST_PROFILE ?= agent

.PHONY: help check test test-lrz test-vct doc-test proofs msrv fmt clippy

help: ## Show the canonical build and test targets
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

check: ## Type-check the default Zakura stack (fast inner loop)
	@CARGO_TARGET_DIR="$(ZAKURA_TARGET_DIR)" \
		cargo check $(APP_PACKAGES) --all-targets --features test-fixtures --locked

test: ## Run the default Zakura test suite
	@CARGO_TARGET_DIR="$(ZAKURA_TARGET_DIR)" \
		cargo nextest run -P $(NEXTEST_PROFILE) $(APP_PACKAGES) \
		--features test-fixtures --locked

doc-test: ## Run documentation tests (nextest cannot run these)
	@CARGO_TARGET_DIR="$(ZAKURA_TARGET_DIR)" \
		cargo test $(APP_PACKAGES) --doc --features test-fixtures --locked

test-lrz: ## Run the LRZ Ironwood / NU6.3 test suite
	@CARGO_TARGET_DIR="$(LRZ_TARGET_DIR)" \
		cargo nextest run -P $(NEXTEST_PROFILE) $(APP_PACKAGES) \
		--all-targets --no-default-features --features lrz --locked

test-vct: ## Run the vote-commitment-tree crates on both backends
	@CARGO_TARGET_DIR="$(VCT_TARGET_DIR)" \
		cargo nextest run -P $(NEXTEST_PROFILE) $(VCT_PACKAGES) \
		--all-targets --no-default-features \
		--features vote-commitment-tree/lrz,vote-commitment-tree-client/lrz,vote-commitment-tree-client/cli \
		--locked
	@CARGO_TARGET_DIR="$(VCT_TARGET_DIR)" \
		cargo nextest run -P $(NEXTEST_PROFILE) $(VCT_PACKAGES) \
		--all-targets --features vote-commitment-tree-client/cli --locked

proofs: ## Run the #[ignore] Halo2 proof tests (release only; slow)
	@CARGO_TARGET_DIR="$(ZAKURA_TARGET_DIR)" \
		cargo nextest run -P $(NEXTEST_PROFILE) --release -p zcash_voting \
		--locked --run-ignored ignored-only
	@CARGO_TARGET_DIR="$(LRZ_TARGET_DIR)" \
		cargo nextest run -P $(NEXTEST_PROFILE) --release -p zcash_voting \
		--no-default-features --features lrz --locked --run-ignored ignored-only

msrv: ## Check every package at the 1.91 MSRV
	@CARGO_TARGET_DIR="$(ZAKURA_TARGET_DIR)/msrv" \
		cargo +1.91.0 check $(APP_PACKAGES) --all-targets --features test-fixtures --locked
	@CARGO_TARGET_DIR="$(LRZ_TARGET_DIR)/msrv" \
		cargo +1.91.0 check $(APP_PACKAGES) --all-targets \
		--no-default-features --features lrz --locked
	@CARGO_TARGET_DIR="$(VCT_TARGET_DIR)/msrv" \
		cargo +1.91.0 check $(VCT_PACKAGES) --all-targets \
		--features vote-commitment-tree-client/cli --locked

fmt: ## Check formatting
	@cargo fmt --all --check

clippy: ## Lint the default Zakura stack
	@CARGO_TARGET_DIR="$(ZAKURA_TARGET_DIR)" \
		cargo clippy $(APP_PACKAGES) --all-targets --features test-fixtures --locked

# Staging crash-recovery conformance. Deliberately not in APP_PACKAGES: this
# package drives a real staging round over the network and kills its own child
# processes, so it must never join `check`, `test`, or CI's hermetic jobs. It
# shares the Zakura target dir so it reuses the main build's artifacts.
RECOVERY_CONFORMANCE_PACKAGE = -p recovery-conformance

recovery-conformance-check: ## Type-check the staging crash-recovery suite
	@CARGO_TARGET_DIR="$(ZAKURA_TARGET_DIR)" \
		cargo clippy $(RECOVERY_CONFORMANCE_PACKAGE) --all-targets --locked

recovery-conformance: ## Run the staging crash-recovery suite (network, slow)
	@CARGO_TARGET_DIR="$(ZAKURA_TARGET_DIR)" \
		cargo nextest run -P $(NEXTEST_PROFILE) $(RECOVERY_CONFORMANCE_PACKAGE) --locked
