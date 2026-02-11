BINARY = zallyd
HOME_DIR = $(HOME)/.zallyd

.PHONY: install init start clean build fmt lint test test-unit test-integration circuits fixtures test-halo2

## install: Build and install the zallyd binary to $GOPATH/bin
install:
	go install ./cmd/zallyd

## build: Build the zallyd binary locally
build:
	go build -o $(BINARY) ./cmd/zallyd

## init: Initialize a single-validator chain (wipes existing data)
init: install
	bash scripts/init.sh

## start: Start the chain
start:
	$(BINARY) start --home $(HOME_DIR)

## clean: Remove chain data directory
clean:
	rm -rf $(HOME_DIR)
	rm -f $(BINARY)

## fmt: Format Go code
fmt:
	go fmt ./...

## lint: Run Go vet
lint:
	go vet ./...

## test-unit: Keeper, validation, codec, module unit tests (fast, parallel)
test-unit:
	go test -count=1 -race -parallel=4 ./x/vote/... ./api/...

## test-integration: Full ABCI pipeline integration tests (in-process chain)
test-integration:
	go test -count=1 -race -timeout 5m ./app/...

## test: Run all tests (Go only, no Rust dependency)
test: test-unit test-integration

# ---------------------------------------------------------------------------
# Halo2 / Rust circuit targets
# ---------------------------------------------------------------------------

## circuits: Build the Rust static library (requires cargo)
circuits:
	cargo build --release --manifest-path circuits/Cargo.toml

## circuits-test: Run Rust circuit unit tests
circuits-test:
	cargo test --release --manifest-path circuits/Cargo.toml

## fixtures: Regenerate proof fixture files in crypto/zkp/testdata/ (requires circuits build)
fixtures: circuits
	cargo test --release --manifest-path circuits/Cargo.toml -- generate_fixtures --ignored --nocapture

## test-halo2: Run Go tests that use real Halo2 verification via CGo (requires circuits)
test-halo2: circuits
	go test -tags halo2 -count=1 -v ./crypto/zkp/halo2/...
