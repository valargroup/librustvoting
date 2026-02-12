# zcash-vote

A Rust library and toolset for Zcash Orchard-based voting with zero-knowledge proofs. Provides nullifier set management, ballot creation/validation, and Merkle tree operations for privacy-preserving elections on the Zcash blockchain.

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

### Database Schema

The `nfs` table stores one row per Orchard nullifier:

```sql
CREATE TABLE nfs(
    id_nf INTEGER PRIMARY KEY NOT NULL,
    election INTEGER NOT NULL,    -- 0 for the global nullifier set
    hash BLOB NOT NULL            -- 32-byte nullifier
);
CREATE UNIQUE INDEX idx_nfs_hash ON nfs(hash);
```

The `properties` table tracks ingestion state:

```sql
CREATE TABLE properties(
    id_property INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    value TEXT NOT NULL
);
-- key: 'last_nf_height' -> last fully synced block height
```

## Library

The core library (`zcash-vote`) provides:

- **`db`** -- SQLite schema creation, nullifier/note/commitment storage
- **`trees`** -- Nullifier range tree construction and Merkle root computation
- **`download`** -- Compact block streaming from lightwalletd via gRPC
- **`decrypt`** -- Orchard note decryption with viewing keys
- **`election`** -- Election parameter management and domain derivation
- **`validate`** -- Key validation (mnemonic / UFVK)
- **`address`** -- Zcash voting address handling

## Building

```bash
# Build everything
cargo build --release

# Run tests
cargo test
```

Requires Rust with the 2021 edition. SQLite is bundled (compiled from source via `rusqlite`'s `bundled` feature) -- no system SQLite installation needed.
