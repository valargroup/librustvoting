//! Building the voter's wallet database by scanning the chain.
//!
//! Shielded notes cannot be derived from a seed — they are found by
//! trial-decrypting blocks — so a wallet that has never been scanned holds
//! nothing, however well funded its addresses are on chain. This module creates
//! the `zcash_client_sqlite` database that `select_notes_with_lwd` reads, and
//! fills it by scanning a bounded range of testnet through lightwalletd.
//!
//! The range is bounded on both ends for a reason. Nothing before NU6.3 can
//! hold a votable Ironwood note, and nothing after the round's snapshot height
//! is in the round's nullifier set, so scanning outside
//! `[activation, snapshot]` cannot change which notes the round can use.
//!
//! Blocks are fetched a chunk at a time rather than all at once. `BlockSource`
//! is a synchronous trait and lightwalletd streams asynchronously, so each
//! chunk is collected in memory and then scanned; chunking is what keeps that
//! buffer bounded across a range of tens of thousands of blocks.

use anyhow::{Context, Result};
use secrecy::SecretVec;

use zcash_voting::backend::zcash_client_backend::{
    data_api::{
        chain::{error::Error as ChainError, scan_cached_blocks, BlockSource, ChainState},
        scanning::ScanPriority,
        Account as _, AccountBirthday, WalletRead, WalletWrite,
    },
    proto::{compact_formats::CompactBlock, service::BlockId, service::BlockRange},
};
use zcash_voting::backend::zcash_client_sqlite::{util::SystemClock, WalletDb};
use zcash_voting::Network;

/// How many blocks are fetched and scanned at a time.
///
/// Bounds the in-memory buffer. Large enough that per-chunk overhead is
/// negligible over tens of thousands of blocks, small enough that a testnet
/// chunk stays comfortably small.
const CHUNK_BLOCKS: u32 = 2_000;

/// A wallet database ready for note selection.
pub struct SyncedWallet {
    pub path: std::path::PathBuf,
    pub account_uuid: String,
    pub scanned_to: u64,
}

/// Compact blocks held in memory for one chunk.
///
/// `scan_cached_blocks` drives a synchronous `BlockSource`, so the async fetch
/// is completed before scanning begins rather than interleaved with it.
struct ChunkSource {
    blocks: Vec<CompactBlock>,
}

impl BlockSource for ChunkSource {
    type Error = anyhow::Error;

    fn with_blocks<F, WalletErrT>(
        &self,
        from_height: Option<zcash_protocol::consensus::BlockHeight>,
        limit: Option<usize>,
        mut with_block: F,
    ) -> Result<(), ChainError<WalletErrT, Self::Error>>
    where
        F: FnMut(CompactBlock) -> Result<(), ChainError<WalletErrT, Self::Error>>,
    {
        let start = from_height.map_or(0, u32::from);
        let mut delivered = 0;
        for block in &self.blocks {
            if u32::try_from(block.height).unwrap_or_default() < start {
                continue;
            }
            if limit.is_some_and(|limit| delivered >= limit) {
                break;
            }
            with_block(block.clone())?;
            delivered += 1;
        }
        Ok(())
    }
}

/// Creates the wallet, imports the seed, and scans `[from_height, to_height]`.
///
/// `from_height` should be no earlier than NU6.3 activation and `to_height` no
/// earlier than the round's snapshot, or the notes the round needs will not be
/// in the database.
pub async fn sync_wallet(
    db_path: &std::path::Path,
    seed: &[u8],
    lightwalletd_url: &str,
    network: Network,
    from_height: u64,
    to_height: u64,
) -> Result<SyncedWallet> {
    anyhow::ensure!(from_height < to_height, "empty scan range");

    let mut client = zcash_voting::lwd::open_channel(lightwalletd_url)
        .await
        .map_err(|error| anyhow::anyhow!("opening lightwalletd: {error}"))?;

    // The birthday is the state *before* the first scanned block, so the
    // wallet's commitment tree starts from a frontier the chain agrees with.
    let birthday_state = zcash_voting::lwd::get_tree_state(&mut client, from_height - 1)
        .await
        .map_err(|error| anyhow::anyhow!("fetching birthday tree state: {error}"))?;
    let birthday = AccountBirthday::from_treestate(birthday_state, None)
        .map_err(|error| anyhow::anyhow!("building birthday: {error:?}"))?;

    let mut wallet = WalletDb::for_path(
        db_path,
        network,
        SystemClock,
        voting_crypto_deps::rand::rngs::OsRng,
    )
    .context("creating the wallet database")?;

    let secret = SecretVec::new(seed.to_vec());
    zcash_voting::backend::zcash_client_sqlite::wallet::init::init_wallet_db(
        &mut wallet,
        Some(SecretVec::new(seed.to_vec())),
    )
    .map_err(|error| anyhow::anyhow!("initialising the wallet database: {error:?}"))?;

    let (account, _usk) = wallet
        .import_account_hd("voter", &secret, zip32::AccountId::ZERO, &birthday, None)
        .map_err(|error| anyhow::anyhow!("importing the voter account: {error:?}"))?;
    let account_uuid = account.id().expose_uuid().to_string();

    // Tell the wallet where the chain ends before asking what to scan; scan
    // ranges are derived from the gap between the birthday and the tip.
    wallet
        .update_chain_tip(zcash_protocol::consensus::BlockHeight::from_u32(
            u32::try_from(to_height).context("scan target does not fit in u32")?,
        ))
        .map_err(|error| anyhow::anyhow!("recording the chain tip: {error:?}"))?;

    let mut scanned_to = from_height;
    let mut cursor = from_height;
    while cursor <= to_height {
        let end = (cursor + u64::from(CHUNK_BLOCKS) - 1).min(to_height);
        let blocks = fetch_range(&mut client, cursor, end).await?;
        if blocks.is_empty() {
            break;
        }

        let prior = zcash_voting::lwd::get_tree_state(&mut client, cursor - 1)
            .await
            .map_err(|error| anyhow::anyhow!("fetching chain state: {error}"))?;
        let chain_state: ChainState = prior
            .to_chain_state()
            .map_err(|error| anyhow::anyhow!("converting chain state: {error:?}"))?;

        let count = blocks.len();
        let source = ChunkSource { blocks };
        scan_cached_blocks(
            &network,
            &source,
            &mut wallet,
            zcash_protocol::consensus::BlockHeight::from_u32(
                u32::try_from(cursor).context("scan cursor does not fit in u32")?,
            ),
            &chain_state,
            count,
        )
        .map_err(|error| anyhow::anyhow!("scanning {cursor}..={end}: {error:?}"))?;

        scanned_to = end;
        cursor = end + 1;
    }

    Ok(SyncedWallet {
        path: db_path.to_path_buf(),
        account_uuid,
        scanned_to,
    })
}

/// Streams one inclusive block range from lightwalletd.
async fn fetch_range(
    client: &mut zcash_voting::backend::zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient<tonic::transport::Channel>,
    start: u64,
    end: u64,
) -> Result<Vec<CompactBlock>> {
    let range = BlockRange {
        start: Some(BlockId {
            height: start,
            hash: vec![],
        }),
        end: Some(BlockId {
            height: end,
            hash: vec![],
        }),
        ..Default::default()
    };
    let mut stream = client
        .get_block_range(range)
        .await
        .with_context(|| format!("requesting blocks {start}..={end}"))?
        .into_inner();

    let mut blocks = Vec::new();
    while let Some(block) = stream
        .message()
        .await
        .with_context(|| format!("streaming blocks {start}..={end}"))?
    {
        blocks.push(block);
    }
    Ok(blocks)
}

/// Whether any scan range still needs attention.
pub fn scan_complete<W: WalletRead>(wallet: &W) -> bool {
    wallet.suggest_scan_ranges().is_ok_and(|ranges| {
        ranges
            .iter()
            .all(|range| range.priority() == ScanPriority::Scanned)
    })
}
