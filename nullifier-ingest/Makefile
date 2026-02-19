# nullifier-ingest
# Top-level Makefile — delegates to imt-tree and service subcrates
#
# Storage: flat binary files (no SQLite).
#
#   nullifiers.bin         – append-only raw 32-byte nullifier blobs
#   nullifiers.checkpoint  – 16-byte (height LE, offset LE) crash-recovery marker
#   nullifiers.tree        – cached full Merkle tree (optional sidecar)
#
# Incremental vs full-resync
# ──────────────────────────
# `make ingest` is incremental — it resumes from the checkpoint and appends
# new nullifiers. The tree sidecar is left untouched, so the running server
# continues to serve the old tree until restarted.
#
# `make ingest-resync` also ingests incrementally, but deletes the tree
# sidecar afterwards (INVALIDATE_TREE=1). The next `make serve` will rebuild
# the Merkle tree from the full flat file. This is a sequential-read + parallel
# Fp::from_repr, so even 50M nullifiers (~1.6 GB) rebuilds in seconds.

IMT_DIR     := imt-tree
SERVICE_DIR := service

# ── Configuration (override with env vars) ───────────────────────────
DATA_DIR      ?= .
LWD_URL       ?= https://zec.rocks:443
PORT          ?= 3000
BOOTSTRAP_URL ?= https://vote.fra1.digitaloceanspaces.com
SYNC_HEIGHT   ?=

# Validate SYNC_HEIGHT and build the MAX_HEIGHT env fragment for the ingest binary.
# If unset, ingest runs to chain tip.  If set, it must be a multiple of 10.
ifdef SYNC_HEIGHT
  ifneq ($(shell expr $(SYNC_HEIGHT) % 10),0)
    $(error SYNC_HEIGHT must be a multiple of 10, got $(SYNC_HEIGHT))
  endif
  _MAX_HEIGHT_ENV := MAX_HEIGHT=$(SYNC_HEIGHT)
else
  _MAX_HEIGHT_ENV :=
endif

# ── Targets ──────────────────────────────────────────────────────────

.PHONY: ingest ingest-resync bootstrap test-proof build test test-integration clean status serve serve-deploy help

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

build: ## Build all binaries (release)
	cd $(SERVICE_DIR) && cargo build --release

bootstrap: ## Download nullifier files from bootstrap URL if not present in DATA_DIR
	@if [ ! -f "$(DATA_DIR)/nullifiers.checkpoint" ]; then \
		echo "Bootstrap: nullifier files not found in $(DATA_DIR), downloading from $(BOOTSTRAP_URL)..."; \
		mkdir -p "$(DATA_DIR)"; \
		wget -q --show-progress -O "$(DATA_DIR)/nullifiers.bin"        "$(BOOTSTRAP_URL)/nullifiers.bin"; \
		wget -q --show-progress -O "$(DATA_DIR)/nullifiers.checkpoint" "$(BOOTSTRAP_URL)/nullifiers.checkpoint"; \
		wget -q --show-progress -O "$(DATA_DIR)/nullifiers.tree"       "$(BOOTSTRAP_URL)/nullifiers.tree"; \
		echo "Bootstrap complete."; \
	else \
		echo "Bootstrap: nullifier files already present in $(DATA_DIR), skipping."; \
	fi

ingest: ## Ingest nullifiers incrementally up to SYNC_HEIGHT (or chain tip if unset)
	cd $(SERVICE_DIR) && DATA_DIR=$(DATA_DIR) LWD_URL=$(LWD_URL) $(_MAX_HEIGHT_ENV) cargo run --release --bin ingest-nfs

ingest-resync: ## Ingest nullifiers up to SYNC_HEIGHT and delete stale tree sidecar so server rebuilds
	cd $(SERVICE_DIR) && DATA_DIR=$(DATA_DIR) LWD_URL=$(LWD_URL) INVALIDATE_TREE=1 $(_MAX_HEIGHT_ENV) cargo run --release --bin ingest-nfs

test-proof: ## Run exclusion proof verification against ingested data
	cd $(SERVICE_DIR) && DATA_DIR=$(DATA_DIR) cargo run --release --bin test-non-inclusion

serve: ## Start the exclusion proof query server
	cd $(SERVICE_DIR) && DATA_DIR=$(DATA_DIR) PORT=$(PORT) cargo run --release --bin query-server

# Same binary and env as CI deploy; use for local testing before pushing.
DEPLOY_DIR ?= nullifier-service
serve-deploy: build ## Build release and run query-server from DEPLOY_DIR
	@mkdir -p $(DEPLOY_DIR)
	cd $(SERVICE_DIR) && DATA_DIR=../$(DEPLOY_DIR) PORT=$(PORT) ./target/release/query-server

test: ## Run unit tests for all subcrates
	cd $(IMT_DIR) && cargo test --lib
	cd $(SERVICE_DIR) && cargo test --lib

test-integration: ## Run IMT ↔ delegation-circuit ZK integration test
	cd $(IMT_DIR) && cargo test --test imt_circuit_integration -- --nocapture

status: ## Show ingestion progress (nullifier count + last synced height)
	@NF="$(DATA_DIR)/nullifiers.bin"; CP="$(DATA_DIR)/nullifiers.checkpoint"; \
	TREE="$(DATA_DIR)/nullifiers.tree"; \
	echo "Data directory: $(DATA_DIR)"; \
	if [ -f "$$NF" ]; then \
		SIZE=$$(ls -lh "$$NF" | awk '{print $$5}'); \
		BYTES=$$(wc -c < "$$NF" | tr -d ' '); \
		COUNT=$$((BYTES / 32)); \
		echo "  nullifiers.bin: $$COUNT nullifiers ($$SIZE)"; \
	else \
		echo "  nullifiers.bin: not found"; \
	fi; \
	if [ -f "$$CP" ]; then \
		HEIGHT=$$(od -An -t u8 -j 0 -N 8 "$$CP" | tr -d ' '); \
		OFFSET=$$(od -An -t u8 -j 8 -N 8 "$$CP" | tr -d ' '); \
		echo "  checkpoint: height=$$HEIGHT offset=$$OFFSET"; \
	else \
		echo "  checkpoint: none"; \
	fi; \
	if [ -f "$$TREE" ]; then \
		TSIZE=$$(ls -lh "$$TREE" | awk '{print $$5}'); \
		echo "  nullifiers.tree: $$TSIZE (sidecar)"; \
	else \
		echo "  nullifiers.tree: not present (will rebuild on serve)"; \
	fi

clean: ## Remove built artifacts and data files
	cd $(IMT_DIR) && cargo clean
	cd $(SERVICE_DIR) && cargo clean
	rm -f $(DATA_DIR)/nullifiers.bin $(DATA_DIR)/nullifiers.checkpoint $(DATA_DIR)/nullifiers.tree
