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
//! So a round admits one run, and a second caller is turned away rather than
//! queued: the work it wanted is already being done, and a run can last until
//! vote end, so waiting for the first to finish would block the caller for
//! hours to then repeat a run whose work is complete.
//!
//! **Turned away, but not while the holder is on its way out.** The dangerous
//! case is not two live runs, it is a run replacing a cancelled one: a host
//! that cancels a run and starts its replacement can arrive between the cancel
//! and the holder's return, and turning that replacement away would leave the
//! round with no run at all — nothing restarts it until the next lifecycle
//! event, which may be after vote end. So a caller that finds the round held
//! waits [`HANDOFF_WINDOW`] for it to be released before concluding a run is
//! really active. A departing run returns well inside that; a live one does
//! not, and costs the caller the window.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex, PoisonError, Weak},
    time::Duration,
};

/// How long a caller waits for a held round to be released before treating the
/// holder as a live run.
///
/// Sized for a run that has already observed cancellation and is unwinding —
/// which is a return, not a timer, since cancellation is observed between
/// passes, during the wait, and inside a pass. It is not a queue: a live run
/// holds its round for as long as it drives it, and the wait only delays this
/// caller's `AlreadyDriving`.
const HANDOFF_WINDOW: Duration = Duration::from_secs(1);
/// Interval for observing release, and for observing this caller's own
/// cancellation while it waits.
const RELEASE_CHECK: Duration = Duration::from_millis(50);

/// The round one run belongs to.
///
/// Wallet-qualified because two wallets hold separate share rows for one round
/// id, and sidecar-qualified because two independently opened databases do
/// too — a run over one of them cannot touch the other's rows, so they are not
/// each other's concurrency.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct RoundKey {
    sidecar_id: u64,
    wallet_id: String,
    round_id: String,
}

impl RoundKey {
    pub(super) fn new(sidecar_id: u64, wallet_id: &str, round_id: &str) -> Self {
        Self {
            sidecar_id,
            wallet_id: wallet_id.to_string(),
            round_id: round_id.to_string(),
        }
    }
}

/// Rounds with a run in flight, each held only as long as its admission is.
///
/// Weak, so a finished, cancelled, dropped, or panicking run releases its
/// round without the registry having to be told; entries are swept on the next
/// claim.
static ACTIVE_RUNS: LazyLock<Mutex<HashMap<RoundKey, Weak<()>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Proof that this run owns its round. Releases it when dropped.
pub(super) struct RunAdmission {
    _token: Arc<()>,
}

/// What a caller found when it asked for a round.
pub(super) enum RoundClaim {
    Admitted(RunAdmission),
    /// A run held the round for the whole handoff window, so it is live rather
    /// than departing.
    HeldByALiveRun,
    /// The caller was cancelled while waiting for the round.
    Interrupted,
}

/// Claims `round` for one run, waiting out a departing holder.
///
/// `interrupted` is this caller's own stop signal, observed while it waits so
/// a host draining the caller does not wait out the window.
pub(super) async fn claim_round(
    round: &RoundKey,
    interrupted: &(dyn Fn() -> bool + Send + Sync),
) -> RoundClaim {
    let give_up_at = tokio::time::Instant::now() + HANDOFF_WINDOW;
    loop {
        if let Some(admission) = try_claim(round) {
            return RoundClaim::Admitted(admission);
        }
        if interrupted() {
            return RoundClaim::Interrupted;
        }
        let left = give_up_at.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return RoundClaim::HeldByALiveRun;
        }
        tokio::time::sleep(RELEASE_CHECK.min(left)).await;
    }
}

/// One attempt, with no waiting.
///
/// A poisoned registry is recovered rather than reported: the map holds only
/// weak handles, so a panic while holding it leaves no half-written invariant,
/// and a driver run has no error channel to report one through.
fn try_claim(round: &RoundKey) -> Option<RunAdmission> {
    let mut active = ACTIVE_RUNS.lock().unwrap_or_else(PoisonError::into_inner);
    active.retain(|_, run| run.strong_count() > 0);
    if active.contains_key(round) {
        return None;
    }
    let token = Arc::new(());
    active.insert(round.clone(), Arc::downgrade(&token));
    Some(RunAdmission { _token: token })
}
