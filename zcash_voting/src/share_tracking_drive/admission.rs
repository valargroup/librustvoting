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
//! event, which may be after vote end.
//!
//! So the holder's own liveness is recorded with its claim, not inferred from
//! how long it takes to unwind. A holder is *live* while its control is
//! uncancelled and still on the epoch it was admitted under, and *departing*
//! once either changes. A caller that finds a live holder is turned away at
//! once; a caller that finds a departing one waits for it to release the round
//! and takes over. That is a decision about state rather than a race against a
//! timeout: a departing holder that is descheduled, blocked in a synchronous
//! reporter, or slow to unwind still hands the round over, and a live holder
//! never costs a caller a wait.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex, PoisonError, Weak},
    time::Duration,
};

use crate::ChainSubmissionControl;

/// Interval for observing a departing holder's release, and for observing this
/// caller's own cancellation while it waits.
///
/// The same shape and interval as `lock_share_operation_or_cancel`, which
/// waits for a share lock the same way: unbounded in principle, because the
/// thing waited on is another task on its way out, and cancellable every tick
/// so a host draining this caller never waits the release out.
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

/// The holder of a round, as a waiting caller sees it.
///
/// Its clone of the holder's control shares the holder's cancellation and
/// epoch state, so liveness is read rather than guessed.
struct Holder {
    control: ChainSubmissionControl,
    entry_epoch: u64,
}

impl Holder {
    /// True while the holder's run is still the host operation it was admitted
    /// under. False once the host cancelled it or moved on — from that moment
    /// the run is unwinding, however long that takes.
    fn is_live(&self) -> bool {
        !self.control.is_cancelled() && self.control.operation_epoch() == self.entry_epoch
    }
}

/// Rounds with a run in flight, each held only as long as its admission is.
///
/// Weak, so a finished, cancelled, dropped, or panicking run releases its
/// round without the registry having to be told; entries are swept on the next
/// claim.
static ACTIVE_RUNS: LazyLock<Mutex<HashMap<RoundKey, Weak<Holder>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Proof that this run owns its round. Releases it when dropped.
pub(super) struct RunAdmission {
    _token: Arc<Holder>,
}

/// What a caller found when it asked for a round.
pub(super) enum RoundClaim {
    Admitted(RunAdmission),
    /// A live run holds the round.
    HeldByALiveRun,
    /// The caller was cancelled while waiting for a departing holder.
    Interrupted,
}

/// Claims `round` for the run `control` and `entry_epoch` belong to, waiting
/// for a departing holder to release it.
///
/// `interrupted` is this caller's own stop signal, observed every tick so a
/// host draining the caller never waits a release out.
pub(super) async fn claim_round(
    round: &RoundKey,
    control: &ChainSubmissionControl,
    entry_epoch: u64,
    interrupted: &(dyn Fn() -> bool + Send + Sync),
) -> RoundClaim {
    loop {
        match try_claim(round, control, entry_epoch) {
            Claim::Admitted(admission) => return RoundClaim::Admitted(admission),
            Claim::HeldByALiveRun => return RoundClaim::HeldByALiveRun,
            Claim::HeldByADepartingRun => {}
        }
        if interrupted() {
            return RoundClaim::Interrupted;
        }
        tokio::time::sleep(RELEASE_CHECK).await;
    }
}

enum Claim {
    Admitted(RunAdmission),
    HeldByALiveRun,
    HeldByADepartingRun,
}

/// One attempt, with no waiting.
///
/// A poisoned registry is recovered rather than reported: the map holds only
/// weak handles, so a panic while holding it leaves no half-written invariant,
/// and a driver run has no error channel to report one through.
fn try_claim(round: &RoundKey, control: &ChainSubmissionControl, entry_epoch: u64) -> Claim {
    let mut active = ACTIVE_RUNS.lock().unwrap_or_else(PoisonError::into_inner);
    active.retain(|_, run| run.strong_count() > 0);
    if let Some(holder) = active.get(round).and_then(Weak::upgrade) {
        return if holder.is_live() {
            Claim::HeldByALiveRun
        } else {
            Claim::HeldByADepartingRun
        };
    }
    let token = Arc::new(Holder {
        control: control.clone(),
        entry_epoch,
    });
    active.insert(round.clone(), Arc::downgrade(&token));
    Claim::Admitted(RunAdmission { _token: token })
}
