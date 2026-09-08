//! One tracking run at a time per round.
//!
//! A host starts runs on lifecycle events — unlock, foreground, a round
//! becoming active — and those can overlap. The pass's per-share locks keep
//! two runs from mutating one share at once, but they do not keep the runs
//! themselves apart: two runs walking the same round interleave across
//! *different* shares, so each re-polls a share the other has just answered
//! and the round's helper traffic doubles for no additional progress. A pass
//! is also meant to plan from the complete previous pass, which an overlapping
//! run breaks.
//!
//! So a round admits one run. The second caller is turned away immediately
//! rather than queued: the work it wanted is already being done, and a run can
//! last until vote end, so waiting for the first to finish would block the
//! caller for hours to then repeat a run whose work is complete.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex, PoisonError, Weak},
};

/// The round one run belongs to. Wallet-qualified, because two wallets in one
/// process hold separate share rows for the same round id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RunKey {
    wallet_id: String,
    round_id: String,
}

/// Rounds with a run in flight, each held only as long as its admission is.
///
/// Weak, so a finished, cancelled, or panicking run releases its round without
/// the registry having to be told; entries are swept on the next admission.
static ACTIVE_RUNS: LazyLock<Mutex<HashMap<RunKey, Weak<()>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Proof that this run owns its round. Releases it when dropped.
pub(super) struct RunAdmission {
    _token: Arc<()>,
}

/// Claims `round_id` for one run, or `None` when a run already holds it.
///
/// A poisoned registry is recovered rather than reported: the map holds only
/// weak handles, so a panic while holding it leaves no half-written invariant,
/// and a driver run has no error channel to report one through.
pub(super) fn admit_run(wallet_id: &str, round_id: &str) -> Option<RunAdmission> {
    let key = RunKey {
        wallet_id: wallet_id.to_string(),
        round_id: round_id.to_string(),
    };
    let mut active = ACTIVE_RUNS.lock().unwrap_or_else(PoisonError::into_inner);
    active.retain(|_, run| run.strong_count() > 0);
    if active.contains_key(&key) {
        return None;
    }
    let token = Arc::new(());
    active.insert(key, Arc::downgrade(&token));
    Some(RunAdmission { _token: token })
}
