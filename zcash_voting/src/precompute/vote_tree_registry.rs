//! Process-local registry of the vote-tree clients a wallet has open.
//!
//! One [`crate::tree_sync::VoteTreeSync`] is kept per sidecar connection,
//! wallet id, and transport. Keying by transport means two executors for one
//! wallet that bind different routes each keep their own client and their
//! own synced tree state; neither one's sync discards the other's. Calls that
//! name no transport (the public `sync_vote_tree`, `van_witness`, and
//! round-scoped `reset_vote_tree`) are steered to the client that already
//! holds the round, so a sync followed by a witness on the standalone path
//! lands on the same state even when another executor synced in between.
//!
//! An entry lives while its sidecar connection has a handle. A routed client
//! additionally lives while some caller holds the transport it was built
//! over or while it holds any round's tree state: a caller that moved its
//! only transport clone into `sync_vote_tree_with` still needs that sync's
//! state for `van_witness`. Once its rounds are reset and nothing can name
//! its transport again, the client is pruned on the next registry access.
//!
//! The registry mutex is a leaf: it is never held across a sync, a round
//! lock, or any other lock.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, OnceLock, Weak,
    },
};

use vote_commitment_tree_client::transport::Transport;

use crate::{
    round::VotingDb,
    tree_sync::{CachedRoundState, VoteTreeSync},
    types::VotingError,
};

/// One wallet client together with what keeps it reachable.
struct VoteTreeEntry {
    sync: Arc<VoteTreeSync>,
    /// The host transport the client was built over, or `None` for the SDK's
    /// direct HTTP transport. Held weakly: the client itself keeps the
    /// transport alive, so whether any caller still holds it is read off the
    /// client, not off this field.
    transport: Option<Weak<dyn Transport>>,
    /// The sidecar connection the entry belongs to; a reopened sidecar gets a
    /// new id, so an entry whose handles are all gone is unreachable.
    connection: Weak<crate::storage::SidecarConnection>,
    /// Tie-breaker for calls that name no transport: the client most recently
    /// handed out wins, so an unrouted call after a routed sync stays on the
    /// route rather than falling back to the direct transport.
    last_used: u64,
}

impl VoteTreeEntry {
    fn is_reachable(&self) -> bool {
        let connection_is_live = self.connection.strong_count() > 0;
        let transport_is_held = self.transport.is_none() || self.sync.transport_is_shared();
        let holds_round_state = !self.sync.cached_rounds().is_empty();
        connection_is_live && (transport_is_held || holds_round_state)
    }

    fn routes_over(&self, requested: &Arc<dyn Transport>) -> bool {
        self.transport.as_ref().is_some_and(|bound| {
            std::ptr::eq(
                Arc::as_ptr(requested) as *const (),
                Weak::as_ptr(bound) as *const (),
            )
        })
    }

    fn touch(&mut self) -> Arc<VoteTreeSync> {
        self.last_used = USE_COUNTER.fetch_add(1, Ordering::Relaxed);
        Arc::clone(&self.sync)
    }
}

/// The sidecar connection plus the wallet id. Two independently opened
/// sidecars that use the same wallet id must not share tree state or each
/// other's transport.
pub(super) type WalletKey = (u64, String);

pub(super) fn wallet_key(db: &VotingDb) -> WalletKey {
    (db.connection_id(), db.wallet_id())
}

static REGISTRY: OnceLock<Mutex<HashMap<WalletKey, Vec<VoteTreeEntry>>>> = OnceLock::new();
static USE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Locks the registry and prunes entries nothing can reach any more.
fn registry() -> Result<MutexGuard<'static, HashMap<WalletKey, Vec<VoteTreeEntry>>>, VotingError> {
    let mut guard = REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|e| VotingError::Internal {
            message: format!("vote tree sync registry lock poisoned: {e}"),
        })?;
    guard.retain(|_, entries| {
        entries.retain(VoteTreeEntry::is_reachable);
        !entries.is_empty()
    });
    Ok(guard)
}

fn new_entry(db: &VotingDb, transport: Option<Arc<dyn Transport>>) -> VoteTreeEntry {
    let sync = Arc::new(match &transport {
        Some(transport) => VoteTreeSync::with_transport(Arc::clone(transport)),
        None => VoteTreeSync::new(),
    });
    VoteTreeEntry {
        sync,
        transport: transport.as_ref().map(Arc::downgrade),
        connection: Arc::downgrade(&db.shared_connection()),
        last_used: 0,
    }
}

/// The wallet's client over `transport`, created on first use.
///
/// `Some(transport)` is only ever served by a client built over that same
/// transport, so the wallet's vote-tree traffic never travels a route the
/// caller did not ask for. `None` means "no particular route": it reuses the
/// client most recently handed out, whatever its transport, and creates one
/// over the direct HTTP transport only when the wallet has none.
pub(crate) fn vote_tree_for(
    db: &VotingDb,
    transport: Option<Arc<dyn Transport>>,
) -> Result<Arc<VoteTreeSync>, VotingError> {
    let mut registry = registry()?;
    let entries = registry.entry(wallet_key(db)).or_default();
    let existing = match &transport {
        Some(requested) => entries
            .iter_mut()
            .find(|entry| entry.routes_over(requested)),
        None => entries.iter_mut().max_by_key(|entry| entry.last_used),
    };
    if let Some(entry) = existing {
        return Ok(entry.touch());
    }
    entries.push(new_entry(db, transport));
    Ok(entries.last_mut().expect("entry just pushed").touch())
}

/// The wallet's client that holds `round_id`, for a call that named no
/// transport and needs the round's existing state.
///
/// Among the wallet's clients, the one synced furthest on the round wins; a
/// client whose round is mid-sync ranks below any settled one, and a client
/// that never saw the round ranks last. Ties go to the client most recently
/// handed out. A wallet with no client gets one over the direct transport.
pub(super) fn vote_tree_for_round(
    db: &VotingDb,
    round_id: &str,
) -> Result<Arc<VoteTreeSync>, VotingError> {
    let mut registry = registry()?;
    let entries = registry.entry(wallet_key(db)).or_default();
    let holder = entries.iter_mut().max_by_key(|entry| {
        let rank = match entry.sync.cached_round_state(round_id) {
            CachedRoundState::SyncedTo(height) => (2, height),
            CachedRoundState::Syncing => (1, 0),
            CachedRoundState::Absent => (0, 0),
        };
        (rank, entry.last_used)
    });
    if let Some(entry) = holder {
        return Ok(entry.touch());
    }
    entries.push(new_entry(db, None));
    Ok(entries.last_mut().expect("entry just pushed").touch())
}

/// Drops `round_id`'s state on every client the wallet has, so no client
/// serves a stale round after a caller asked for it to be forgotten.
pub(super) fn reset_round(db: &VotingDb, round_id: &str) -> Result<(), VotingError> {
    let syncs: Vec<Arc<VoteTreeSync>> = registry()?
        .get(&wallet_key(db))
        .map(|entries| {
            entries
                .iter()
                .map(|entry| Arc::clone(&entry.sync))
                .collect()
        })
        .unwrap_or_default();
    for sync in syncs {
        sync.reset(round_id)?;
    }
    Ok(())
}

/// Forgets every client the wallet has, transports included, so the next
/// sync binds afresh.
pub(super) fn forget_wallet(db: &VotingDb) -> Result<(), VotingError> {
    let entries = registry()?.remove(&wallet_key(db)).unwrap_or_default();
    for entry in entries {
        entry.sync.reset("")?;
    }
    Ok(())
}

/// Rounds held in memory by any of the wallet's clients, deduplicated and
/// sorted. Observes only; it never creates a client.
pub(super) fn cached_rounds(db: &VotingDb) -> Vec<String> {
    let Ok(registry) = registry() else {
        return Vec::new();
    };
    let mut rounds: Vec<String> = registry
        .get(&wallet_key(db))
        .into_iter()
        .flatten()
        .flat_map(|entry| entry.sync.cached_rounds())
        .collect();
    rounds.sort();
    rounds.dedup();
    rounds
}
