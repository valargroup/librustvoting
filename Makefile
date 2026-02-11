BINARY = zallyd
HOME_DIR = $(HOME)/.zallyd

.PHONY: install init start clean build fmt lint test test-unit test-integration

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

## test: Run all tests
test: test-unit test-integration
