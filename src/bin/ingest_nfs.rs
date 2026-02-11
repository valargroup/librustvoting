use std::env;

use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use tonic::transport::{Certificate, ClientTlsConfig};
use tonic::Request;

use zcash_vote::db;
use zcash_vote::rpc::{
    compact_tx_streamer_client::CompactTxStreamerClient, BlockId, BlockRange, ChainSpec,
};

/// NU5 (Orchard) activation height on Zcash mainnet
const NU5_ACTIVATION_HEIGHT: u64 = 1_687_104;

/// Default lightwalletd endpoint
const DEFAULT_LWD_URL: &str = "https://zec.rocks:443";

/// Default SQLite database path
const DEFAULT_DB_PATH: &str = "nullifiers.db";

/// How many blocks to request per gRPC streaming call
const BATCH_SIZE: u64 = 10_000;

#[tokio::main]
async fn main() -> Result<()> {
    let lwd_url = env::var("LWD_URL").unwrap_or_else(|_| DEFAULT_LWD_URL.to_string());
    let db_path = env::var("DB_PATH").unwrap_or_else(|_| DEFAULT_DB_PATH.to_string());

    // --- Set up SQLite ---
    println!("Opening SQLite database: {}", db_path);
    let manager = SqliteConnectionManager::file(&db_path);
    let pool = Pool::new(manager)?;
    let connection = pool.get()?;

    // Create schema (tables are IF NOT EXISTS, safe to re-run)
    db::create_schema(&connection)?;

    // Performance pragmas for bulk ingestion
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA cache_size = -256000;
         PRAGMA temp_store = MEMORY;
         PRAGMA mmap_size = 2147483648;",
    )?;

    // --- Migrate nfs table to remove column-level UNIQUE constraint ---
    // SQLite won't let us drop sqlite_autoindex_nfs_1 (column constraint).
    // Instead, recreate the table without UNIQUE on hash, move data over,
    // and we'll build a standalone UNIQUE INDEX at the very end.
    migrate_nfs_table(&connection)?;

    // --- Connect to lightwalletd ---
    println!("Connecting to lightwalletd: {}", lwd_url);
    let mut ep = tonic::transport::Channel::from_shared(lwd_url.clone())?;
    if lwd_url.starts_with("https") {
        let pem = include_bytes!("../ca.pem");
        let ca = Certificate::from_pem(pem);
        let tls = ClientTlsConfig::new().ca_certificate(ca);
        ep = ep.tls_config(tls)?;
    }
    let mut client = CompactTxStreamerClient::connect(ep).await?;

    // Get chain tip
    let latest = client
        .get_latest_block(Request::new(ChainSpec {}))
        .await?;
    let chain_tip = latest.into_inner().height;
    println!("Chain tip: {}", chain_tip);

    // --- Determine resume point ---
    let last_synced = db::load_prop(&connection, "last_nf_height")?
        .and_then(|h| h.parse::<u64>().ok());

    let start = match last_synced {
        Some(h) if h >= NU5_ACTIVATION_HEIGHT => {
            println!("Resuming from height {} (previously synced)", h);
            h
        }
        _ => {
            println!(
                "Starting fresh from NU5 activation height {}",
                NU5_ACTIVATION_HEIGHT
            );
            NU5_ACTIVATION_HEIGHT
        }
    };

    if start >= chain_tip {
        println!("Already up to date!");
        rebuild_index(&connection)?;
        return Ok(());
    }

    let total_blocks = chain_tip - start;
    println!(
        "Downloading nullifiers: heights {} -> {} ({} blocks)",
        start + 1,
        chain_tip,
        total_blocks
    );

    // Use election = 0 as a generic "all nullifiers" bucket
    let id_election = 0u32;
    let mut current = start + 1;
    let mut total_nfs: u64 = 0;
    let mut blocks_processed: u64 = 0;
    let t_start = std::time::Instant::now();

    while current <= chain_tip {
        let batch_end = std::cmp::min(current + BATCH_SIZE - 1, chain_tip);

        let mut stream = client
            .get_block_range(Request::new(BlockRange {
                start: Some(BlockId {
                    height: current,
                    hash: vec![],
                }),
                end: Some(BlockId {
                    height: batch_end,
                    hash: vec![],
                }),
                spam_filter_threshold: 0,
            }))
            .await?
            .into_inner();

        // Buffer all nullifiers from this gRPC stream in memory,
        // then flush to SQLite in one transaction with a prepared statement.
        let mut nf_buffer: Vec<Vec<u8>> = Vec::new();
        let mut last_height = current;

        while let Some(block) = stream.message().await? {
            last_height = block.height;
            for tx in block.vtx {
                for a in tx.actions {
                    nf_buffer.push(a.nullifier);
                }
            }
        }

        let batch_nfs = nf_buffer.len() as u64;

        // Write all buffered nullifiers in one transaction with a reused prepared stmt
        connection.execute_batch("BEGIN")?;
        {
            let mut stmt = connection
                .prepare_cached("INSERT INTO nfs(election, hash) VALUES (?1, ?2)")?;
            for nf in &nf_buffer {
                stmt.execute(params![id_election, nf])?;
            }
        }
        // Save progress checkpoint and commit atomically
        db::store_prop(&connection, "last_nf_height", &last_height.to_string())?;
        connection.execute_batch("COMMIT")?;

        // Free buffer memory immediately for spam batches
        drop(nf_buffer);

        total_nfs += batch_nfs;
        blocks_processed += last_height - current + 1;
        let elapsed = t_start.elapsed().as_secs_f64();
        let bps = if elapsed > 0.0 {
            blocks_processed as f64 / elapsed
        } else {
            0.0
        };
        let remaining = (total_blocks - blocks_processed) as f64 / bps.max(1.0);

        println!(
            "  height {}/{} | +{} nfs | {} total nfs | {:.0} blocks/s | ~{:.0}s remaining",
            last_height, chain_tip, batch_nfs, total_nfs, bps, remaining
        );

        current = batch_end + 1;
    }

    let elapsed = t_start.elapsed();
    println!(
        "\nIngestion done! {} nullifiers across {} blocks in {:.1}s",
        total_nfs, blocks_processed, elapsed.as_secs_f64()
    );
    println!("Database: {}", db_path);

    // Quick stats
    let count: u64 = connection.query_row(
        "SELECT COUNT(*) FROM nfs WHERE election = 0",
        [],
        |r| r.get(0),
    )?;
    println!("Total nullifiers in DB: {}", count);

    // Rebuild the unique index now that all data is loaded
    rebuild_index(&connection)?;

    Ok(())
}

/// Migrate the nfs table: remove the column-level UNIQUE constraint on hash
/// so that bulk inserts don't pay the cost of index maintenance on every row.
/// Data is preserved. This is idempotent -- if already migrated, it's a no-op.
fn migrate_nfs_table(
    connection: &r2d2::PooledConnection<SqliteConnectionManager>,
) -> Result<()> {
    // Check if the old autoindex still exists (meaning we haven't migrated yet)
    let has_autoindex: bool = connection.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master
         WHERE type='index' AND name='sqlite_autoindex_nfs_1'",
        [],
        |r| r.get(0),
    )?;

    if !has_autoindex {
        println!("nfs table already migrated (no autoindex). Skipping migration.");
        return Ok(());
    }

    let row_count: u64 =
        connection.query_row("SELECT COUNT(*) FROM nfs", [], |r| r.get(0))?;
    println!(
        "Migrating nfs table ({} rows): removing column UNIQUE constraint for bulk perf...",
        row_count
    );
    let t = std::time::Instant::now();

    connection.execute_batch(
        "CREATE TABLE nfs_new(
            id_nf INTEGER PRIMARY KEY NOT NULL,
            election INTEGER NOT NULL,
            hash BLOB NOT NULL
         );
         INSERT INTO nfs_new SELECT * FROM nfs;
         DROP TABLE nfs;
         ALTER TABLE nfs_new RENAME TO nfs;",
    )?;

    println!(
        "Migration complete in {:.1}s. Unique index will be built after ingestion finishes.",
        t.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Recreate the unique index on nfs.hash after bulk loading completes.
fn rebuild_index(
    connection: &r2d2::PooledConnection<SqliteConnectionManager>,
) -> Result<()> {
    // Check if any unique index on hash already exists
    let has_index: bool = connection.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master
         WHERE type='index' AND tbl_name='nfs'
         AND (name='idx_nfs_hash' OR name='sqlite_autoindex_nfs_1')",
        [],
        |r| r.get(0),
    )?;

    if has_index {
        println!("Unique index on nfs.hash already exists.");
        return Ok(());
    }

    println!("Building unique index on nfs.hash...");
    let t = std::time::Instant::now();
    connection.execute_batch("CREATE UNIQUE INDEX idx_nfs_hash ON nfs(hash);")?;
    println!("Index built in {:.1}s", t.elapsed().as_secs_f64());
    Ok(())
}
