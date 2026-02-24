# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Protocol Documentation Reference

**Always consult the documentation files first** — Claude's training data may lack sufficient detail on these specific protocols.

### Zcash Protocol Spec

- `docs/papers/zcash-protocol-index.md` - Section index with line ranges (read this FIRST)
- `docs/papers/zcash-protocol.tex` - Full protocol spec in LaTeX (read specific line ranges from index)

## Code Style

- Prefer explicit type annotations for public APIs
- Use `#[must_use]` on functions returning Results
- Avoid `unwrap()` in library code, use `expect()` with message or propagate errors
- Do not delete existing comments unless they are factually wrong or reference removed code. When modifying code near comments, preserve them. When adding new code alongside existing commented code, add comments for the new code at the same level of detail.

## Debugging

- Add `dbg!()` for quick inspection (remove before commit)
- Run single test with output: `cargo test test_name -- --nocapture`
- Check NTT correctness: compare against naive polynomial multiplication in tests

## Pull Request Guidelines

When writing PR descriptions, always describe changes relative to the target branch (e.g., main), not the iterative steps taken during development. The PR description should reflect the actual diff - what code exists in the target branch vs what code exists in the PR branch. Avoid phrases like "renamed X to Y" unless X actually exists in the target branch.

**After pushing to a branch**, always check if there is an open PR for that branch. If so, review whether the PR title and description still accurately reflect the current state of the changes. If the pushed commits have changed the scope or nature of the PR, update the title and/or description accordingly using `gh pr edit`.

## Database Migrations

Do not create new migration files (e.g., `002_*.sql`). This is a pre-production codebase — modify the existing `001_init.sql` directly. Only create separate migrations if explicitly asked.

## FFI Builds

There are two xcframework build targets in `zcash-voting-ffi/`:

- **`make dev-incr`** — Incremental build (~30s–2min). Use for Rust-only changes that don't touch the FFI interface (e.g., adding logs, fixing logic in `librustvoting` or `orchard`, tweaking circuit code). Skips clean and bindings regeneration, just recompiles changed crates and copies the `.a` into the xcframework.

- **`make dev`** — Full clean rebuild (~8min). Use when:
  - The FFI public API changed (`zcash-voting-ffi/rust/src/lib.rs`, uniffi exports, types exposed through FFI)
  - You need to regenerate Swift bindings (`Sources/ZcashVotingFFI/zcash_voting_ffi.swift`)
  - The incremental build produces errors (stale artifacts)

After modifying the FFI public API, you **must** run `make dev` and commit the regenerated Swift file and xcframework binaries alongside the Rust changes.

## Local IMT (Nullifier Ingest) Service

The IMT service lives in `nullifier-ingest/`. Data files (`nullifiers.bin`, `nullifiers.checkpoint`, `nullifiers.tree`) are stored at the `nullifier-ingest/` root, not in `nullifier-ingest/service/`.

### Common operations (run from `nullifier-ingest/`):

- **Check status:** `make status`
- **Ingest to a specific height:** `make ingest SYNC_HEIGHT=<height>` (must be a multiple of 10)
- **Ingest to chain tip:** `make ingest`
- **Start query server:** `make serve` (runs on port 3000)
- **Bootstrap from CDN:** `make bootstrap` (downloads pre-built snapshot files if not present)

### Key notes:

- `SYNC_HEIGHT` must be a **multiple of 10**
- After ingesting new blocks, you **must delete the stale tree sidecar** before restarting the server, otherwise the server loads the old tree and returns height mismatches (HTTP 502). The full sequence is:
  1. `make ingest SYNC_HEIGHT=<height>`
  2. `rm nullifiers.tree`
  3. `make serve` (rebuilds the tree from `.bin` on startup, ~3–5 min for 50M nullifiers)
- Alternatively, use `make ingest-resync SYNC_HEIGHT=<height>` which deletes the tree sidecar automatically after ingestion
- `eprintln!` from Rust code shows up in the Xcode debug console when testing the iOS app

## Local Chain Setup

Starting all services for local development: `make up` from the repo root. This starts the chain (`zallyd`), bootstraps nullifiers, and runs the IMT query server.

### Full local setup sequence

The correct sequence to start everything from scratch:

1. `make up` (from repo root) — inits chain, bootstraps + ingests nullifiers, starts zallyd + IMT query server
2. `make ceremony` (from `sdk/`) — runs EA key ceremony (required before creating voting rounds)
3. `npm run dev` (from `shielded_vote_generator_ui/`) — starts admin UI on port 5173
4. Rebuild iOS app in Xcode and run

If the IMT query server returns HTTP 502 with a height mismatch, the tree sidecar is stale. Fix: `pkill query-server && rm nullifier-ingest/nullifiers.tree && make ingest-serve`. The `make up` target now deletes the stale tree automatically, but manual `make ingest` followed by `make ingest-serve` still requires the manual `rm`.

### Important: `make install-ffi` vs `make install`

- **`make install`** builds `zallyd` **without** halo2/redpallas support. The embedded helper server will be **disabled** (logs: "helper server disabled: binary built without halo2 support"). Votes submitted from the iOS app will fail with **HTTP 503 "helper unavailable"**.
- **`make install-ffi`** builds `zallyd` **with** halo2 and redpallas build tags. This is required for the helper server to run. **Always use `make install-ffi`** when rebuilding `zallyd` for local testing.
- `make init` already calls `install-ffi`, so a fresh `make up` is fine. The issue arises when you manually run `make install` to pick up a Go code change — this silently downgrades the binary.

### GOBIN and version managers (mise, asdf, etc.)

The Makefiles set `export GOBIN := $(HOME)/go/bin` so that `go install` puts the binary in `~/go/bin`, matching the `PATH` they export. If you use a Go version manager like mise that overrides `GOBIN` to its own directory, the Makefile's explicit `GOBIN` takes precedence, preventing stale binaries in `~/go/bin` from shadowing the freshly built one.

### Ceremony requirement

Before creating a voting round, the EA key ceremony must be in CONFIRMED status. Run `make ceremony` from `sdk/` after `make up`. Check status: `curl -s http://localhost:1318/zally/v1/ceremony`.

## Protocol Documentation

When modifying circuit logic (in `orchard/`, `librustvoting/`, or `sdk/circuits/`), the corresponding documentation in the Obsidian gitbook (the `shielded_vote_book` repository) must also be updated. The book is served live and describes the circuit structure — any protocol change that affects conditions, public inputs, witness fields, or hash parameters must be reflected there.

## Code Change Guidelines

**Never consider backwards compatibility** unless explicitly told to do so. Feel free to rename functions, change APIs, delete unused code, and refactor without worrying about breaking existing consumers. This is a research codebase where clean code matters more than stability.

- Describe changes relative to main branch, not iterative development steps
- Include benchmark results if touching performance-critical code
- Security-impacting changes require extra review
- PR description should reflect actual diff (what exists in main vs PR branch)
