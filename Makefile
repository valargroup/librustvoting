# Root Makefile
SDK_DIR     = sdk
INGEST_DIR  = nullifier-ingest

export GOBIN := $(HOME)/go/bin
export PATH := /usr/local/go/bin:$(GOBIN):$(PATH)

# Optional upper bound for nullifier ingestion (must be a multiple of 10).
# Pass as: make up SYNC_HEIGHT=2500000
SYNC_HEIGHT ?=

.PHONY: install install-ffi init start clean build fmt lint \
	test test-unit test-integration test-api test-api-restart test-api-reinit test-e2e \
	fixtures-ts circuits circuits-test fixtures \
	test-halo2 test-halo2-ante test-redpallas test-redpallas-ante test-all-ffi \
	ingest ingest-bootstrap ingest-status ingest-test ingest-proof ingest-clean ingest-serve \
	ingest-test-integration \
	up down status \
	ceremony-prod

install:
	$(MAKE) -C $(SDK_DIR) install

install-ffi:
	$(MAKE) -C $(SDK_DIR) install-ffi

build:
	$(MAKE) -C $(SDK_DIR) build

init:
	$(MAKE) -C $(SDK_DIR) init

start:
	$(MAKE) -C $(SDK_DIR) start

clean:
	$(MAKE) -C $(SDK_DIR) clean

fmt:
	$(MAKE) -C $(SDK_DIR) fmt

lint:
	$(MAKE) -C $(SDK_DIR) lint

test-unit:
	$(MAKE) -C $(SDK_DIR) test-unit

test-integration:
	$(MAKE) -C $(SDK_DIR) test-integration

test:
	$(MAKE) -C $(SDK_DIR) test

test-api:
	$(MAKE) -C $(SDK_DIR) test-api

test-e2e:
	cargo test --release --manifest-path e2e-tests/Cargo.toml -- --nocapture --ignored

test-api-restart:
	$(MAKE) -C $(SDK_DIR) test-api-restart

test-api-reinit:
	$(MAKE) -C $(SDK_DIR) test-api-reinit

fixtures-ts:
	$(MAKE) -C $(SDK_DIR) fixtures-ts

circuits:
	$(MAKE) -C $(SDK_DIR) circuits

circuits-test:
	$(MAKE) -C $(SDK_DIR) circuits-test

fixtures:
	$(MAKE) -C $(SDK_DIR) fixtures

test-halo2:
	$(MAKE) -C $(SDK_DIR) test-halo2

test-halo2-ante:
	$(MAKE) -C $(SDK_DIR) test-halo2-ante

test-redpallas:
	$(MAKE) -C $(SDK_DIR) test-redpallas

test-redpallas-ante:
	$(MAKE) -C $(SDK_DIR) test-redpallas-ante

test-all-ffi:
	$(MAKE) -C $(SDK_DIR) test-all-ffi

# ── Nullifier Ingestion ──────────────────────────────────────────────

ingest-bootstrap: ## Download nullifier bootstrap files if not already present
	$(MAKE) -C $(INGEST_DIR) bootstrap

ingest: ## Ingest Orchard nullifiers up to SYNC_HEIGHT (or chain tip if unset)
	$(MAKE) -C $(INGEST_DIR) ingest SYNC_HEIGHT=$(SYNC_HEIGHT)

ingest-status: ## Show nullifier count, last synced height, file sizes
	$(MAKE) -C $(INGEST_DIR) status

status: ## Show chain node status and nullifier ingestion status
	@echo "=== Chain Node ==="
	@$(HOME)/go/bin/zallyd status --home $(HOME)/.zallyd 2>/dev/null \
		| python3 -c "import sys,json; d=json.load(sys.stdin); s=d.get('sync_info',{}); \
		  print('  moniker    :', d.get('node_info',{}).get('moniker','?')); \
		  print('  block      :', s.get('latest_block_height','?')); \
		  print('  syncing    :', s.get('catching_up','?')); \
		  print('  latest time:', s.get('latest_block_time','?'))" \
		|| echo "  (node not running)"
	@echo ""
	@echo "=== Nullifier Ingest ==="
	@$(MAKE) -s -C $(INGEST_DIR) status

ingest-test: ## Run nullifier-tree unit tests
	$(MAKE) -C $(INGEST_DIR) test

ingest-proof: ## Run exclusion proof verification against ingested data
	$(MAKE) -C $(INGEST_DIR) test-proof

ingest-serve: ## Start the nullifier PIR server
	$(MAKE) -C $(INGEST_DIR) serve

ingest-clean: ## Remove nullifier build artifacts and database
	$(MAKE) -C $(INGEST_DIR) clean

ingest-test-integration: ## Run IMT ↔ delegation-circuit ZK integration test
	$(MAKE) -C $(INGEST_DIR) test-integration

# ── Ceremony ─────────────────────────────────────────────────────────

CEREMONY_PROD_ARGS ?=

ceremony-prod: ## Run ceremony.sh with production chain env vars; pass command via CEREMONY_PROD_ARGS or as trailing args
	@ZALLY_HOME=/opt/zally-chain/.zallyd-val1 \
	ZALLY_NODE_RPC=tcp://127.0.0.1:26157 \
	ZALLY_REST_API=http://localhost:1418 \
	ZALLY_FROM=validator \
	ZALLY_KEYRING=test \
	./ceremony.sh $(CEREMONY_PROD_ARGS) $(filter-out $@,$(MAKECMDGOALS))

# absorb any extra word-goals (e.g. "reset") so make doesn't treat them as targets
%:
	@:

# ── Full Stack ───────────────────────────────────────────────────────

down: ## Stop all running zallyd and nf-server processes
	@KILLED=""; \
	pkill zallyd       2>/dev/null && KILLED="$$KILLED zallyd"       || true; \
	pkill nf-server    2>/dev/null && KILLED="$$KILLED nf-server"    || true; \
	if [ -n "$$KILLED" ]; then \
		printf '\033[32mStopped:%s\033[0m\n' "$$KILLED"; \
	else \
		echo "No running processes found."; \
	fi

up: ## Init SDK, bootstrap+ingest nullifiers, then run ingest-serve and start in parallel
	@PROCS=""; \
	pgrep zallyd       > /dev/null 2>&1 && PROCS="$$PROCS zallyd"; \
	pgrep nf-server    > /dev/null 2>&1 && PROCS="$$PROCS nf-server"; \
	if [ -n "$$PROCS" ]; then \
		printf '\033[31mERROR: the following processes are already running:%s\n' "$$PROCS"; \
		printf 'Stop them first before running "make up".\033[0m\n'; \
		exit 1; \
	fi
	$(MAKE) init
	$(MAKE) ingest-bootstrap
	$(MAKE) ingest
	$(MAKE) -C $(INGEST_DIR) export-nf
	@nohup $(MAKE) ingest-serve > ingest-serve.log 2>&1 & \
	nohup $(MAKE) start > zallyd.log 2>&1 & \
	printf '\033[32mStarted: ingest-serve → ingest-serve.log | zallyd → zallyd.log\033[0m\n'
