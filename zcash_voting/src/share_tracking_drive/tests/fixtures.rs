//! Shared fixtures: an in-memory sidecar, a host source that can be varied
//! per pass, and a reporter that records every driver event.

pub(super) use std::sync::atomic::{AtomicU64, Ordering};
pub(super) use std::sync::{Arc, Mutex};
pub(super) use std::time::Duration;

pub(super) use crate::{
    helper::{client::HelperClient, health::HelperHealth},
    round::VotingDb,
    share_tracking_drive::{
        ShareTrackingDrivePolicy, ShareTrackingDriver, ShareTrackingEvent,
        ShareTrackingHostContext, ShareTrackingHostSource, ShareTrackingQuiescence,
        ShareTrackingReporter, ShareTrackingRunReport,
    },
    ChainSubmissionControl,
};

/// A round id is a canonical Pallas field element in little-endian hex, so an
/// arbitrary 64-character string is rejected. This is `1`.
pub(super) const ROUND_ID: &str =
    "0100000000000000000000000000000000000000000000000000000000000000";
/// A second round in the same wallet. This is `2`.
pub(super) const OTHER_ROUND_ID: &str =
    "0200000000000000000000000000000000000000000000000000000000000000";
pub(super) const NOW: u64 = 1_000;
pub(super) const VOTE_END: u64 = 1_000_000;

/// A helper whose URL is well formed but never contacted.
///
/// Every test here keeps its shares before their status-check time, so the
/// fleet is only validated, never called. A fleet must still be non-empty:
/// tracking rejects an empty one before it touches storage.
pub(super) fn fleet() -> Vec<String> {
    vec!["https://helper.example".to_string()]
}

pub(super) fn client() -> HelperClient {
    HelperClient::new(Arc::new(UnreachableTransport), HelperHealth::default())
}

/// Fails every request. Reaching it means a test stopped being a pure pacing
/// test, so failing loudly is the point.
struct UnreachableTransport;

impl crate::helper::transport::HelperTransport for UnreachableTransport {
    fn get<'a>(
        &'a self,
        url: &'a str,
        _timeout: Duration,
    ) -> crate::helper::transport::HelperFuture<'a> {
        Box::pin(async move {
            Err(crate::helper::transport::HelperTransportError::Transport(
                format!("unexpected GET {url} in a pacing test"),
            ))
        })
    }

    fn post_json<'a>(
        &'a self,
        url: &'a str,
        _body: Vec<u8>,
        _timeout: Duration,
    ) -> crate::helper::transport::HelperFuture<'a> {
        Box::pin(async move {
            Err(crate::helper::transport::HelperTransportError::Transport(
                format!("unexpected POST {url} in a pacing test"),
            ))
        })
    }
}

/// A wallet id no other test in this process shares.
///
/// A driver admits one run per wallet and round, and its per-share locks are
/// wallet-qualified too, so tests that reused one id would contend through
/// process-wide state and turn each other's runs away.
pub(super) fn unique_wallet_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "share-tracking-drive-wallet-{}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// A sidecar with no share rows at all.
pub(super) fn empty_db() -> VotingDb {
    let db = VotingDb::open_in_memory().unwrap();
    db.set_wallet_id(&unique_wallet_id());
    db
}

/// A sidecar holding one unconfirmed share whose status check is still
/// `seconds_ahead` away, so a pass has something to wait for and nothing to
/// poll.
pub(super) fn db_with_pending_share(seconds_ahead: u64) -> VotingDb {
    let db = empty_db();
    seed_round_with_pending_share(&db, ROUND_ID, seconds_ahead);
    db
}

/// Adds one more round with its own pending share to an existing sidecar.
///
/// Two rounds in one wallet are what shows that a run holds its own round
/// rather than the wallet.
pub(super) fn seed_round_with_pending_share(db: &VotingDb, round_id: &str, seconds_ahead: u64) {
    seed_voted_bundle(db, round_id);
    db.record_share_delegation(round_id, 0, 1, 0, &fleet(), &[0u8; 32], NOW + seconds_ahead)
        .unwrap();
}

/// The round, bundle and stored vote a share row hangs off.
///
/// A share is a child of a vote, so recording one without these fails the
/// foreign key rather than producing an orphan the driver could poll.
fn seed_voted_bundle(db: &VotingDb, round_id: &str) {
    db.create_round(
        crate::Network::Testnet,
        &crate::round::RoundParams {
            vote_round_id: round_id.to_string(),
            snapshot_height: 1000,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0xAA; 32],
            nullifier_imt_root: vec![0xBB; 32],
        },
        None,
    )
    .unwrap();
    db.ensure_bundles(
        round_id,
        &[crate::types::NoteInfo {
            commitment: vec![0x01; 32],
            nullifier: vec![0x02; 32],
            value: crate::governance::BALLOT_DIVISOR,
            position: 0,
            diversifier: vec![0x03; 11],
            rho: vec![0x04; 32],
            rseed: vec![0x05; 32],
            scope: 0,
            ufvk_str: "uview1test".to_string(),
        }],
    )
    .unwrap();
    db.set_ballot_intent(round_id, 1, crate::session::Decision::Choice(2), 3)
        .unwrap();
    crate::storage::queries::store_vote(
        &db.conn(),
        round_id,
        &db.wallet_id(),
        0,
        1,
        2,
        &[0xCA; 32],
    )
    .unwrap();
}

/// Host inputs, optionally different for each pass.
pub(super) struct ScriptedHost {
    contexts: Mutex<Vec<ShareTrackingHostContext>>,
    pub(super) reads: Mutex<u32>,
}

impl ScriptedHost {
    /// The same context for every pass.
    pub(super) fn fixed(vote_end: Option<u64>) -> Self {
        Self::scripted(vec![ShareTrackingHostContext {
            configured_helper_urls: fleet(),
            now_seconds: NOW,
            vote_end_time_seconds: vote_end,
        }])
    }

    /// One context per pass; the last one repeats once the script runs out.
    pub(super) fn scripted(contexts: Vec<ShareTrackingHostContext>) -> Self {
        Self {
            contexts: Mutex::new(contexts),
            reads: Mutex::new(0),
        }
    }
}

impl ShareTrackingHostSource for ScriptedHost {
    fn host_context(&self) -> ShareTrackingHostContext {
        let mut reads = self.reads.lock().unwrap();
        let contexts = self.contexts.lock().unwrap();
        let index = (*reads as usize).min(contexts.len() - 1);
        *reads += 1;
        contexts[index].clone()
    }
}

/// Moves the control to another operation epoch once the run is under way.
///
/// The host source is read once per pass, which makes it the one deterministic
/// place a test can change the world mid-run without racing the driver.
pub(super) struct EpochBumpingHost<'a> {
    control: &'a ChainSubmissionControl,
    reads: Mutex<u32>,
    bump_after: u32,
}

impl<'a> EpochBumpingHost<'a> {
    pub(super) fn after_first_pass(control: &'a ChainSubmissionControl) -> Self {
        Self {
            control,
            reads: Mutex::new(0),
            bump_after: 1,
        }
    }
}

impl ShareTrackingHostSource for EpochBumpingHost<'_> {
    fn host_context(&self) -> ShareTrackingHostContext {
        let mut reads = self.reads.lock().unwrap();
        *reads += 1;
        if *reads > self.bump_after {
            self.control
                .set_operation_epoch(self.control.operation_epoch() + 1);
        }
        ShareTrackingHostContext {
            configured_helper_urls: fleet(),
            now_seconds: NOW,
            vote_end_time_seconds: Some(VOTE_END),
        }
    }
}

#[derive(Default)]
pub(super) struct RecordingReporter {
    pub(super) events: Mutex<Vec<ShareTrackingEvent>>,
}

impl RecordingReporter {
    pub(super) fn delays(&self) -> Vec<Duration> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                ShareTrackingEvent::AwaitingNextPass { delay } => Some(*delay),
                _ => None,
            })
            .collect()
    }

    pub(super) fn passes_started(&self) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, ShareTrackingEvent::PassStarted { .. }))
            .count()
    }
}

impl ShareTrackingReporter for RecordingReporter {
    fn report(&self, event: ShareTrackingEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// Runs `db`'s shares to quiescence under `policy`.
pub(super) async fn drive(
    db: &VotingDb,
    host: &ScriptedHost,
    control: &ChainSubmissionControl,
    policy: ShareTrackingDrivePolicy,
) -> (ShareTrackingRunReport, RecordingReporter) {
    drive_with(db, host, control, policy).await
}

/// [`drive`] over any host source.
pub(super) async fn drive_with(
    db: &VotingDb,
    host: &dyn ShareTrackingHostSource,
    control: &ChainSubmissionControl,
    policy: ShareTrackingDrivePolicy,
) -> (ShareTrackingRunReport, RecordingReporter) {
    let client = client();
    let events = RecordingReporter::default();
    let report = ShareTrackingDriver::new(db, &client, ROUND_ID)
        .with_policy(policy)
        .run(host, control, &events)
        .await;
    (report, events)
}
