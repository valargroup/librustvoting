use incrementalmerkletree::{Marking, Position, Retention};
use orchard::tree::MerkleHashOrchard;
use rusqlite::types::Value;
use shardtree::store::{Checkpoint, ShardStore};
use shardtree::ShardTree;
use zcash_client_sqlite::wallet::commitment_tree::{
    SqliteShardStore, create_orchard_tree_tables,
};
use zcash_protocol::consensus::BlockHeight;

use crate::VotingError;

const ORCHARD_SHARD_HEIGHT: u8 = { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 } / 2;

fn err(msg: impl std::fmt::Display) -> VotingError {
    VotingError::WitnessGeneration {
        message: msg.to_string(),
    }
}

/// Generate Orchard Merkle witnesses at a historical frontier.
///
/// Copies the wallet's Orchard shard data into an ephemeral in-memory database,
/// inserts the provided frontier (from lightwalletd) as a checkpoint, and
/// generates a witness for each of the given note positions.
///
/// The wallet DB is strictly read-only — shard data is copied, not modified.
///
/// The wallet tree may have advanced past the snapshot height; this function
/// produces witnesses anchored at the snapshot frontier regardless.
pub fn generate_orchard_witnesses_at_frontier(
    conn: &rusqlite::Connection,
    note_positions: &[Position],
    frontier: incrementalmerkletree::frontier::NonEmptyFrontier<MerkleHashOrchard>,
    checkpoint_height: BlockHeight,
) -> Result<
    Vec<incrementalmerkletree::MerklePath<MerkleHashOrchard, { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 }>>,
    VotingError,
> {
    let frontier_position = frontier.position();

    let mem_conn = rusqlite::Connection::open_in_memory().map_err(err)?;

    create_orchard_tree_tables(&mem_conn).map_err(err)?;

    // Copy shard data from wallet into in-memory DB
    {
        let mut stmt = conn
            .prepare(
                "SELECT shard_index, subtree_end_height, root_hash, shard_data, contains_marked
                 FROM orchard_tree_shards",
            )
            .map_err(err)?;
        let mut rows = stmt.query([]).map_err(err)?;
        while let Some(row) = rows.next().map_err(err)? {
            mem_conn
                .execute(
                    "INSERT INTO orchard_tree_shards
                     (shard_index, subtree_end_height, root_hash, shard_data, contains_marked)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        row.get::<_, Value>(0).map_err(err)?,
                        row.get::<_, Value>(1).map_err(err)?,
                        row.get::<_, Value>(2).map_err(err)?,
                        row.get::<_, Value>(3).map_err(err)?,
                        row.get::<_, Value>(4).map_err(err)?,
                    ],
                )
                .map_err(err)?;
        }
    }

    // Copy cap data
    {
        let mut stmt = conn
            .prepare("SELECT cap_id, cap_data FROM orchard_tree_cap")
            .map_err(err)?;
        let mut rows = stmt.query([]).map_err(err)?;
        while let Some(row) = rows.next().map_err(err)? {
            mem_conn
                .execute(
                    "INSERT INTO orchard_tree_cap (cap_id, cap_data) VALUES (?1, ?2)",
                    rusqlite::params![
                        row.get::<_, Value>(0).map_err(err)?,
                        row.get::<_, Value>(1).map_err(err)?,
                    ],
                )
                .map_err(err)?;
        }
    }

    // Build ShardTree from in-memory store
    let tx = mem_conn.unchecked_transaction().map_err(err)?;

    let store =
        SqliteShardStore::<_, MerkleHashOrchard, ORCHARD_SHARD_HEIGHT>::from_connection(
            &tx, "orchard",
        )
        .map_err(err)?;

    let mut tree = ShardTree::<
        _,
        { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 },
        ORCHARD_SHARD_HEIGHT,
    >::new(store, 100);

    tree.insert_frontier_nodes(
        frontier,
        Retention::Checkpoint {
            id: checkpoint_height,
            marking: Marking::None,
        },
    )
    .map_err(|e| err(format!("failed to insert frontier nodes: {e}")))?;

    tree.store_mut()
        .add_checkpoint(
            checkpoint_height,
            Checkpoint::at_position(frontier_position),
        )
        .map_err(|e| err(format!("failed to add checkpoint: {e}")))?;

    let mut witnesses = Vec::with_capacity(note_positions.len());
    for &pos in note_positions {
        let merkle_path = tree
            .witness_at_checkpoint_id(pos, &checkpoint_height)
            .map_err(|e| {
                err(format!(
                    "failed to generate witness for position {}: {e} \
                     (wallet may need to sync through snapshot height)",
                    u64::from(pos),
                ))
            })?
            .ok_or_else(|| {
                err(format!(
                    "no witness available for position {} \
                     (wallet missing shard data — sync through snapshot height)",
                    u64::from(pos),
                ))
            })?;

        witnesses.push(merkle_path);
    }

    Ok(witnesses)
}
