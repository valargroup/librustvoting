use super::*;
use crate::helper::client::HelperFleetPreflight;
use crate::vote::ConfirmedVote;
use std::sync::atomic::AtomicUsize;
use tokio::sync::Semaphore;

pub(super) const SHARE_COUNT: usize = crate::share_policy::VOTE_COMMITMENT_SHARE_COUNT;

pub(super) struct Fixture {
    pub db: Arc<VotingDb>,
    pub votes: Vec<ConfirmedVote>,
    pub configured: Vec<String>,
}

impl Fixture {
    pub fn new(proposals: u32) -> Self {
        let db = Arc::new(VotingDb::open_in_memory().unwrap());
        Self::seed(db, proposals)
    }

    pub fn with_helpers(proposals: u32, helper_count: usize) -> Self {
        Self::seed_with_helpers(
            Arc::new(VotingDb::open_in_memory().unwrap()),
            proposals,
            helper_count,
        )
    }

    pub fn seed(db: Arc<VotingDb>, proposals: u32) -> Self {
        Self::seed_with_helpers(db, proposals, 1)
    }

    fn seed_with_helpers(db: Arc<VotingDb>, proposals: u32, helper_count: usize) -> Self {
        static WALLET: AtomicUsize = AtomicUsize::new(0);
        db.set_wallet_id(&format!(
            "delivery-queue-{}",
            WALLET.fetch_add(1, Ordering::SeqCst)
        ));
        seed_round_and_bundle(&db);
        for proposal in 1..=proposals {
            let mut recovery = super::super::observability::complete_recovery(proposal);
            // Each proposal has its own commitment and reveal nullifiers.
            recovery.vote_commitment = field_bytes(proposal as u8 + 30);
            super::super::observability::store_recovery(&db, &recovery, true);
        }
        let configured = helpers(helper_count);
        let fleet = HelperFleetPreflight::from_readiness(&configured, &configured).unwrap();
        let roster = (1..=proposals).collect::<Vec<_>>();
        let votes = roster
            .iter()
            .map(|proposal| {
                let vote =
                    crate::vote::CommittedVote::recover(&db, ROUND_ID, 0, *proposal).unwrap();
                vote.prepare_share_delivery(
                    &db,
                    ShareDeliveryPlanningParams {
                        fleet: &fleet,
                        now_seconds: SUBMIT_AT,
                        vote_end_time_seconds: VOTE_END,
                        last_moment_buffer_seconds: None,
                        proposal_ids: &roster,
                    },
                )
                .unwrap();
                vote.confirmed(&db).unwrap().unwrap()
            })
            .collect();
        Self {
            db,
            votes,
            configured,
        }
    }

    pub async fn deliver(
        &self,
        transport: Arc<ScriptedTransport>,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Vec<Result<ShareBatchDeliveryReport, ShareDeliveryFailure>> {
        crate::vote::submit_confirmed_vote_shares(
            &self.votes,
            &self.db,
            &HelperClient::new(transport, HelperHealth::default()),
            ShareDeliverySubmissionParams {
                configured_server_urls: &self.configured,
                now_seconds: SUBMIT_AT,
            },
            cancel,
            &mut |_, _| {},
        )
        .await
        .into_iter()
        .map(|vote| vote.delivery)
        .collect()
    }
}

pub(super) struct ReplyPlan {
    pub delay: Duration,
    pub gate: Option<Arc<Semaphore>>,
    pub status: u16,
}

impl Default for ReplyPlan {
    fn default() -> Self {
        Self {
            delay: Duration::ZERO,
            gate: None,
            status: 200,
        }
    }
}

type PostScript = dyn Fn(&VoteShareWire) -> ReplyPlan + Send + Sync;

pub(super) struct ScriptedTransport {
    script: Box<PostScript>,
    pub started: Mutex<Vec<VoteShareWire>>,
    pub completed: Mutex<Vec<(u32, u32)>>,
    pub active: AtomicUsize,
    pub peak: AtomicUsize,
}

impl ScriptedTransport {
    pub fn new(script: impl Fn(&VoteShareWire) -> ReplyPlan + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
            started: Mutex::new(Vec::new()),
            completed: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        })
    }

    pub fn count(&self) -> usize {
        self.started.lock().unwrap().len()
    }

    pub async fn wait_for(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.count() < count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected share requests were not dispatched");
    }
}

struct ActivePost<'a>(&'a AtomicUsize);
impl Drop for ActivePost<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl HelperTransport for ScriptedTransport {
    fn get<'a>(&'a self, _url: &'a str, _timeout: Duration) -> HelperFuture<'a> {
        Box::pin(async { json_status("ok") })
    }

    fn post_json<'a>(
        &'a self,
        _url: &'a str,
        body: Vec<u8>,
        _timeout: Duration,
    ) -> HelperFuture<'a> {
        Box::pin(async move {
            let wire: VoteShareWire = serde_json::from_slice(&body).unwrap();
            let identity = (wire.proposal_id, wire.share_index);
            let plan = (self.script)(&wire);
            self.started.lock().unwrap().push(wire);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            let _active = ActivePost(&self.active);
            if let Some(gate) = plan.gate {
                gate.acquire().await.unwrap().forget();
            }
            tokio::time::sleep(plan.delay).await;
            self.completed.lock().unwrap().push(identity);
            if plan.status == 200 {
                json_status("queued")
            } else {
                http_status(plan.status)
            }
        })
    }
}

pub(super) fn assert_complete(
    reports: Vec<Result<ShareBatchDeliveryReport, ShareDeliveryFailure>>,
    count: usize,
) {
    assert_eq!(reports.len(), count);
    for report in reports {
        let report = report.unwrap();
        assert_eq!(report.deliveries.len(), SHARE_COUNT);
        assert!(report.pending_share_indices.is_empty());
        assert!(!report.cancelled);
        assert!(report
            .deliveries
            .iter()
            .all(|share| share.submission.accepted_urls.len() == 1));
    }
}

pub(super) fn uncancelled() -> bool {
    false
}
