# nullifier-tree

Ingest every Zcash Orchard nullifier, build a Merkle tree of gap ranges, and serve exclusion proofs that demonstrate a value has never been spent on-chain.

## Files

| File | Purpose |
|---|---|
| `src/lib.rs` | Crate root: re-exports tree module |
| `src/tree.rs` | Gap-range construction (`build_nf_ranges`), leaf commitment (`commit_ranges`), range lookup (`find_range_for_value`), Merkle root (`compute_nf_root`), `NullifierTree` and `ExclusionProof` types, and binary tree caching (`save_tree`/`load_tree`) |
| `src/db.rs` | SQLite schema for the `nullifiers` and `checkpoint` tables |
| `src/download.rs` | TLS-enabled gRPC client for connecting to a lightwalletd node |
| `src/sync_nullifiers.rs` | Parallel block streaming and nullifier extraction from lightwalletd |
| `src/ca.pem` | Embedded CA certificate for lightwalletd TLS (ISRG Root X1) |
| `src/cash.z.wallet.sdk.rpc.rs` | Generated protobuf types for the lightwalletd compact block streaming API |
| `src/bin/ingest_nfs.rs` | Binary that downloads all Orchard nullifiers from the chain into a local SQLite database |
| `src/bin/test_non_inclusion.rs` | Binary that verifies exclusion proofs against the ingested nullifier set |

## Nullifier Ingestion

The `ingest-nfs` binary downloads all Orchard nullifiers from the Zcash blockchain into a local SQLite database. This is required to build the nullifier range tree used for non-inclusion proofs in the voting protocol.

### Quick Start

```bash
cargo run --release --bin ingest-nfs
```

This will:
1. Create (or open) `nullifiers.db` in the current directory
2. Connect to a public lightwalletd server
3. Stream all compact blocks from NU5 activation (height 1,687,104) to the chain tip
4. Extract and store every Orchard nullifier
5. Build a unique index on the nullifier hashes when complete

### Configuration

Environment variables (all optional):

| Variable  | Default                   | Description                    |
|-----------|---------------------------|--------------------------------|
| `LWD_URL` | `https://zec.rocks:443`   | lightwalletd gRPC endpoint     |
| `DB_PATH` | `nullifiers.db`           | SQLite database file path      |

Example with custom settings:

```bash
LWD_URL=https://mainnet.lightwalletd.com:9067 DB_PATH=./data/nfs.db cargo run --release --bin ingest-nfs
```

### Resume Support

The ingestion is fully resumable. A `last_nf_height` checkpoint is saved after every 10,000-block batch. If the process is interrupted (Ctrl+C, crash, etc.), simply re-run the same command and it will pick up where it left off with no data loss.

### Performance Optimizations

The ingestion tool applies several optimizations for bulk loading:

- **Schema migration**: On first run, the column-level `UNIQUE` constraint on `nfs.hash` is converted to a standalone index built only after all data is loaded. This avoids expensive B-tree index maintenance on every INSERT during bulk ingestion.
- **Prepared statements**: SQL statements are compiled once and reused across all inserts.
- **Buffered writes**: Each 10k-block batch is buffered in memory, then flushed in a single SQLite transaction.
- **WAL mode**: Write-Ahead Logging allows concurrent reads while writing.
- **Memory-mapped I/O**: 2 GB mmap window for fast page access.

### Monitoring Progress

While the ingestion is running, you can query the database from another terminal (WAL mode supports concurrent readers):

```bash
# Check last synced height
sqlite3 nullifiers.db "SELECT value FROM properties WHERE name = 'last_nf_height';"

# Count total nullifiers
sqlite3 nullifiers.db "SELECT COUNT(*) FROM nfs;"

# Check database size
ls -lh nullifiers.db
```

### Reference Run (Feb 2026)

Full ingestion from NU5 activation to chain tip (height 3,235,242):

| Metric              | Value           |
|---------------------|-----------------|
| Blocks processed    | 1,548,138       |
| Nullifiers ingested | 49,712,978      |
| Ingestion time      | ~28 minutes     |
| Index build time    | ~2.6 minutes    |
| Total wall time     | ~31 minutes     |
| Final DB size       | 4.0 GB          |
| Avg throughput      | 862 blocks/s    |

The Zcash sandblasting attack periods (heights ~1.7M-1.85M and ~2.0M-2.5M) contain the vast majority of the nullifiers. The ingestion handles these at full speed thanks to the deferred index strategy.

## Exclusion Proofs

An exclusion proof shows that a given nullifier value falls inside a **gap range** (an interval between two adjacent on-chain nullifiers), meaning it has never been spent.

Each leaf in the Merkle tree is a **commitment to a `(low, high)` range pair** — `hash(low, high)` using the same Sinsemilla-based combine as the rest of the tree. Given on-chain nullifiers `n1, n2`, the leaves are:

```
hash(0, n1-1) | hash(n1+1, n2-1) | hash(n2+1, max)
```

A single Merkle path proves a committed range is in the tree. The prover then reveals `(low, high)` and the verifier checks the hash matches and the target value falls within.

```rust
use nullifier_tree::{list_nf_ranges, commit_ranges, find_range_for_value};
use orchard::vote::calculate_merkle_paths;
use rusqlite::Connection;

// 1. Open the nullifier database
let conn = Connection::open("nullifiers.db")?;

// 2. Build gap ranges and commit each (low, high) pair into a leaf hash
let ranges = list_nf_ranges(&conn)?;
let leaves = commit_ranges(&ranges);

// 3. Find which gap range contains your value
let my_nullifier: Fp = /* ... */;
let range_idx = find_range_for_value(&ranges, my_nullifier)
    .expect("value IS an existing nullifier — no exclusion proof possible");

// 4. Get the Merkle path for this leaf
let pos = range_idx as u32;
let (root, paths) = calculate_merkle_paths(0, &[pos], &leaves);

// The proof is:
//   - Merkle path reconstructs to root
//   - hash(low, high) matches the leaf
//   - my_nullifier ∈ [low, high]
```

Run the built-in test binary to verify end-to-end:

```bash
cargo run --release --bin test-non-inclusion
```

### Tree caching

For large nullifier sets, rebuilding ranges from the database is slow. You can cache the computed ranges to a binary file:

```rust
use nullifier_tree::{list_nf_ranges, save_tree, load_tree};

// Build and save
let ranges = list_nf_ranges(&conn)?;
save_tree(Path::new("nf_ranges.bin"), &ranges)?;

// Later, load without touching the database
let ranges = load_tree(Path::new("nf_ranges.bin"))?;
```

Format: `[8-byte LE count][count × 2 × 32-byte Fp representations]` (each range is a `(low, high)` pair).

## Building

```bash
# Build everything
cargo build --release

# Run tests
cargo test
```

Requires Rust with the 2021 edition. SQLite is bundled (compiled from source via `rusqlite`'s `bundled` feature) -- no system SQLite installation needed.
