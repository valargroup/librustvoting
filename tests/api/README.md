# Zally API Tests

Lightweight TypeScript integration tests that hit the chain's REST endpoints (`/zally/v1/*`) to exercise vote round setup and delegation submission.

## Prerequisites

- **Node.js 18+** (uses built-in `fetch`)
- **npm**
- A locally running Zally chain with the API server enabled

## Chain Setup

1. Build and initialize the chain:

   ```bash
   make init
   ```

2. Enable the Cosmos SDK API server in `~/.zallyd/config/app.toml`:

   ```toml
   [api]
   enable = true
   address = "tcp://localhost:1318"
   ```

   Port 1318 is used because 1317 is often occupied by other processes. You can use any free port -- just set `ZALLY_API_URL` accordingly when running tests.

3. Start the chain:

   ```bash
   make start
   ```

   Wait a few seconds for blocks to start producing. The chain must be built **without** `-tags halo2` (the default) so that mock ZKP and RedPallas verifiers are active.

## Proof Fixtures

Delegation tests use real Halo2 toy-circuit proof bytes loaded from `tests/api/fixtures/`. These files are **not** checked into git -- they must be generated from the Rust circuits crate.

### Generate fixtures (requires Rust/Cargo)

From the repository root:

```bash
make fixtures-ts
```

This builds the Rust circuits, generates the binary fixtures under `crypto/zkp/testdata/`, and copies the two relevant files into `tests/api/fixtures/`:

- `toy_valid_proof.bin` -- 1472-byte Halo2 proof for the toy circuit (a=2, b=3, constant=7, c=252)
- `toy_valid_input.bin` -- 32-byte little-endian Pallas Fp encoding of c=252 (used as the `rk` field)

### Regenerating after circuit changes

If the toy circuit in `circuits/src/toy.rs` changes, re-run `make fixtures-ts` to regenerate. The old `.bin` files will be overwritten.

### Without Rust

If you don't have a Rust toolchain, the tests still pass against a default chain build (mock verifiers accept any non-empty proof). A warning is printed to the console:

```
[zally-api-tests] Halo2 fixture files not found in tests/api/fixtures/.
Falling back to mock proof data. Run `make fixtures-ts` to generate real fixtures.
```

## Install

From the repository root:

```bash
cd tests/api
npm install
```

## Run Tests

```bash
# From tests/api/
npm test

# Or from the repository root:
make test-api
```

### Watch Mode

```bash
cd tests/api
npm run test:watch
```

### Custom API URL

If the chain's API server is on a different host or port:

```bash
ZALLY_API_URL=http://localhost:9999 npm test
```

## What's Tested

### Vote Round (`vote-round.test.ts`)

| Test | Endpoint | Expectation |
|------|----------|-------------|
| Setup vote round | `POST /zally/v1/setup-round` | code 0, tx_hash returned |
| Query round after creation | `GET /zally/v1/round/{id}` | Round fields match |
| Missing required fields | `POST /zally/v1/setup-round` | HTTP 400 |
| Empty request body | `POST /zally/v1/setup-round` | HTTP 400 |

### Delegation (`delegation.test.ts`)

| Test | Endpoint | Expectation |
|------|----------|-------------|
| Submit delegation (happy path) | `POST /zally/v1/submit-delegation` | code 0 (real Halo2 proof if fixtures present, mock otherwise) |
| Non-existent round ID | `POST /zally/v1/submit-delegation` | code != 0 (ante rejects) |
| Duplicate nullifiers | `POST /zally/v1/submit-delegation` | code != 0 (nullifier spent) |
| Missing `rk` field | `POST /zally/v1/submit-delegation` | HTTP 400 |
| Empty `proof` field | `POST /zally/v1/submit-delegation` | HTTP 400 |

## Design Notes

- Tests are **idempotent** -- each run generates unique round IDs and nullifiers (seeded from timestamps) so they work against persistent chain state.
- Block wait time is 6 seconds (`BLOCK_WAIT_MS` in `helpers.ts`). Adjust if your chain uses a different block period.
- `@noble/hashes` provides Blake2b for client-side round ID derivation to predict on-chain IDs.
- No Cosmos SDK JS client needed -- the REST API accepts plain JSON with base64-encoded byte fields.
