BINARY = zallyd
HOME_DIR = $(HOME)/.zallyd

.PHONY: install init start clean build fmt lint test

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

## test: Run tests
test:
	go test ./...
