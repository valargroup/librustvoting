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

.PHONY: help check test test-lrz test-vct doc-test proofs msrv fmt clippy \
	recovery-conformance-check recovery-conformance recovery-conformance-crash \
	recovery-conformance-stalls recovery-conformance-fleet

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
RECOVERY_CONFORMANCE_ARGS ?=

.PHONY: recovery-conformance-worker
recovery-conformance-worker: ## Build the exact worker used by live recovery matrices
	@CARGO_TARGET_DIR="$(ZAKURA_TARGET_DIR)" \
		cargo build $(RECOVERY_CONFORMANCE_PACKAGE) --bin recovery-conformance-worker --locked

recovery-conformance-check: ## Type-check the staging crash-recovery suite
	@CARGO_TARGET_DIR="$(ZAKURA_TARGET_DIR)" \
		cargo clippy $(RECOVERY_CONFORMANCE_PACKAGE) --all-targets --locked

recovery-conformance: recovery-conformance-worker ## Run every staging recovery matrix: crash, hang, fleet (network, very slow)
	@CARGO_TARGET_DIR="$(ZAKURA_TARGET_DIR)" \
		cargo nextest run -P $(NEXTEST_PROFILE) $(RECOVERY_CONFORMANCE_PACKAGE) --locked $(RECOVERY_CONFORMANCE_ARGS)

# One axis at a time. The full run provisions roughly thirty-five rounds on
# `svote-1` and takes hours, so a change that can only affect one axis should
# pay for one axis. The hermetic tests run under every one of these.
recovery-conformance-crash: recovery-conformance-worker ## Run only the staging crash matrix (network, slow)
	@CARGO_TARGET_DIR="$(ZAKURA_TARGET_DIR)" \
		cargo nextest run -P $(NEXTEST_PROFILE) $(RECOVERY_CONFORMANCE_PACKAGE) --locked $(RECOVERY_CONFORMANCE_ARGS) \
		-E 'not (binary(stall_conformance) or binary(helper_fleet_conformance))'

recovery-conformance-stalls: recovery-conformance-worker ## Run only the staging hang matrix (network, slow)
	@CARGO_TARGET_DIR="$(ZAKURA_TARGET_DIR)" \
		cargo nextest run -P $(NEXTEST_PROFILE) $(RECOVERY_CONFORMANCE_PACKAGE) --locked $(RECOVERY_CONFORMANCE_ARGS) \
		-E 'not (binary(staging_conformance) or binary(helper_fleet_conformance))'

recovery-conformance-fleet: recovery-conformance-worker ## Run only the staging helper-fleet matrix (network, slow)
	@CARGO_TARGET_DIR="$(ZAKURA_TARGET_DIR)" \
		cargo nextest run -P $(NEXTEST_PROFILE) $(RECOVERY_CONFORMANCE_PACKAGE) --locked $(RECOVERY_CONFORMANCE_ARGS) \
		-E 'not (binary(staging_conformance) or binary(stall_conformance))'

.PHONY: recovery-conformance-unit
recovery-conformance-unit: ## Run hermetic crash-recovery harness tests (no staging)
	@CARGO_TARGET_DIR="$(ZAKURA_TARGET_DIR)" \
		cargo nextest run -P $(NEXTEST_PROFILE) $(RECOVERY_CONFORMANCE_PACKAGE) --locked $(RECOVERY_CONFORMANCE_ARGS) \
		--test stage_taxonomy --test stage_config --test target_chain \
		--test crash_log --test round_shape --test orchestration \
		--test fault_routes --test helper_fleet_plan --test stall_taxonomy \
		--test combined_recovery --test precompute
