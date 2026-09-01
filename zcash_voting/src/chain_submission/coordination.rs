//! Process-local serialization and in-flight reservation ownership.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, OnceLock, Weak},
};

use rusqlite::named_params;

use super::{now_seconds, ChainSubmissionIdentity};
use crate::{storage::VotingDb, types::VotingError};

pub(super) const RESERVATION_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(30);

/// Marks an outstanding reservation as still owned, best effort.
///
/// Deliberately infallible, and deliberately non-blocking. This is a liveness
/// hint for readers that cannot see this process's in-memory registry; failing a
/// submission because the hint could not be written would trade a real
/// transaction for a bookkeeping error. Waiting for the connection would be
/// worse still: it runs inside the same task as the POST, so blocking on the
/// database mutex stops that task being polled, and with it the request
/// deadline this heartbeat exists to keep the reservation inside. A skipped
/// refresh costs the reservation its cross-process coverage only once the grace
/// period elapses, and only if the clock has moved that far.
pub(super) fn refresh_attempt_reservation(db: &VotingDb, wallet_id: &str, attempt_id: i64) {
    let Ok(now) = now_seconds() else {
        return;
    };
    let Some(conn) = db.try_conn() else {
        return;
    };
    // Nor wait on SQLite's own lock: holding this handle's mutex says nothing
    // about another connection or process holding the write lock, and the
    // configured busy timeout would then block this task for seconds — with the
    // POST's deadline among the timers that stop being polled.
    if conn.busy_timeout(std::time::Duration::ZERO).is_err() {
        return;
    }
    let refreshed = conn.execute(
        "UPDATE chain_submission_attempts SET updated_at=:now
          WHERE id=:id AND wallet_id=:wallet_id AND state='attempting'",
        named_params! { ":now": now, ":id": attempt_id, ":wallet_id": wallet_id },
    );
    // Restore the timeout every other caller of this connection depends on.
    let _ = conn.busy_timeout(crate::storage::SQLITE_BUSY_TIMEOUT);
    // A refusal here is the expected outcome under contention, and skipping is
    // what best effort means: the cost is this reservation's cross-process
    // coverage once the grace period elapses, against blocking the request it
    // is meant to protect.
    let _ = refreshed;
}

/// One submission this process has an outstanding POST for.
///
/// Keyed by the durable identity rather than by the journal row's id. Row ids
/// restart per database file, so two handles on different files mint the same
/// id: an id-keyed registry would report one database's expired reservation as
/// live because another currently owns that number, and releasing either guard
/// would uncover the other. The identity is the thing coverage is actually
/// about, and it means the same thing in every database.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct InFlightKey {
    pub(super) wallet_id: String,
    pub(super) round_id: String,
    pub(super) kind: &'static str,
    pub(super) bundle_index: u32,
    pub(super) proposal_key: i64,
    pub(super) batch_digest: Vec<u8>,
}

/// Outstanding reservations, counted so overlapping registrations of one
/// identity cannot uncover each other when the first of them is released.
static IN_FLIGHT_ATTEMPTS: OnceLock<Mutex<BTreeMap<InFlightKey, usize>>> = OnceLock::new();

fn in_flight_registry() -> &'static Mutex<BTreeMap<InFlightKey, usize>> {
    IN_FLIGHT_ATTEMPTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Submissions this process is waiting on a response for in one round, right
/// now.
///
/// Exact and clock-free, which is what keeps a live POST covered across a
/// wall-clock adjustment. The age test in [`CAN_STILL_LEARN_A_HASH`] exists for
/// the reservations this registry cannot know about: another process's, and the
/// crashed ones no registry will ever hold.
pub(super) fn in_flight_for_round(round_id: &str, wallet_id: &str) -> Vec<InFlightKey> {
    let Ok(live) = in_flight_registry().lock() else {
        return Vec::new();
    };
    live.keys()
        .filter(|key| key.round_id == round_id && key.wallet_id == wallet_id)
        .cloned()
        .collect()
}

/// Whether this process has an outstanding POST for a bundle in the pruned
/// range.
pub(crate) fn has_in_flight_at_or_after(round_id: &str, wallet_id: &str, from_index: u32) -> bool {
    in_flight_for_round(round_id, wallet_id)
        .iter()
        .any(|key| key.bundle_index >= from_index)
}

/// How many live registrations one identity currently has.
///
/// The registry is counted, so an overlap shows up here as a value above one.
#[cfg(test)]
pub(super) fn in_flight_count(wallet_id: &str, identity: &ChainSubmissionIdentity) -> usize {
    let key = InFlightKey {
        wallet_id: wallet_id.to_string(),
        round_id: identity.round_id().to_string(),
        kind: identity.kind().as_str(),
        bundle_index: identity.bundle_index(),
        proposal_key: identity.proposal_key(),
        batch_digest: identity.batch_key().to_vec(),
    };
    in_flight_registry()
        .lock()
        .map(|live| live.get(&key).copied().unwrap_or(0))
        .unwrap_or(0)
}

/// Keeps one reservation registered as in flight for as long as it is held.
///
/// Releasing on drop rather than at each exit means an early return, an error,
/// or a panic between the POST and its classification cannot leave an entry
/// pinning coverage for the life of the process.
pub(super) struct InFlightAttempt(InFlightKey);

impl InFlightAttempt {
    pub(super) fn register(wallet_id: &str, identity: &ChainSubmissionIdentity) -> Self {
        let key = InFlightKey {
            wallet_id: wallet_id.to_string(),
            round_id: identity.round_id().to_string(),
            kind: identity.kind().as_str(),
            bundle_index: identity.bundle_index(),
            proposal_key: identity.proposal_key(),
            batch_digest: identity.batch_key().to_vec(),
        };
        if let Ok(mut live) = in_flight_registry().lock() {
            *live.entry(key.clone()).or_insert(0) += 1;
        }
        Self(key)
    }
}

impl Drop for InFlightAttempt {
    fn drop(&mut self) {
        if let Ok(mut live) = in_flight_registry().lock() {
            if let Some(count) = live.get_mut(&self.0) {
                *count -= 1;
                if *count == 0 {
                    live.remove(&self.0);
                }
            }
        }
    }
}

/// How long a reservation may go untouched before no process can still be
/// waiting on it.
///
/// A row is `attempting` only between its reservation and the response
/// classification that follows its POST, so it is bounded by that call's request
/// deadline. Deriving this from [`MAX_REQUEST_TIMEOUT`] rather than picking a
/// number keeps the two from drifting apart: a host cannot configure a deadline
/// that outlives the grace period and make a live reservation look abandoned.
/// The doubling leaves room for the database work and scheduling either side of
/// the request itself.
///
/// It bounds the freeze a crashed reservation causes by minutes rather than by
/// the life of the round.
pub(super) const INTERRUPTED_RESERVATION_GRACE_SECS: i64 =
    2 * crate::chain::MAX_REQUEST_TIMEOUT.as_secs() as i64;

/// How far ahead of now a reservation's stamp may be and still be believed.
///
/// Only enough to absorb second-granularity rounding between the stamp and the
/// read. Anything further ahead is a clock that stepped backward, not a
/// reservation from the future.
pub(super) const FUTURE_STAMP_TOLERANCE_SECS: i64 = RESERVATION_HEARTBEAT.as_secs() as i64;

#[cfg(test)]
pub(crate) fn interrupted_reservation_grace_secs() -> i64 {
    INTERRUPTED_RESERVATION_GRACE_SECS
}

pub(super) type OperationLock = Arc<tokio::sync::Mutex<()>>;

/// Returns the process-wide lock serializing operations for one identity.
///
/// The registry holds weak references and the caller holds the only strong one
/// for the duration of its operation. Two concurrent operations on the same
/// identity still share one mutex, because the second upgrades the entry while
/// the first is holding it; once no operation is left, the entry becomes
/// reclaimable. A long-lived wallet moves through many rounds and proposals, so
/// keeping a strong reference per identity forever would grow without bound.
pub(super) fn identity_operation_lock(key: &str) -> Result<OperationLock, VotingError> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|error| VotingError::Internal {
            message: format!("chain operation lock registry poisoned: {error}"),
        })?;
    if let Some(live) = locks.get(key).and_then(Weak::upgrade) {
        return Ok(live);
    }
    locks.retain(|_, entry| entry.strong_count() > 0);
    let lock: OperationLock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key.to_string(), Arc::downgrade(&lock));
    Ok(lock)
}
