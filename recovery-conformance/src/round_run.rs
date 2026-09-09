//! Constructing and driving one round, for every mode the suite needs.
//!
//! Three runs share this code: an armed run that dies at a crash stage, an
//! unarmed run that resumes a crashed sidecar to quiescence, and an unarmed run
//! over a fresh round that produces the control the others are compared
//! against. They must share it — a control built by different code would not be
//! a control, it would be a second implementation whose agreement proves
//! nothing.
//!
//! Every mode runs in a child process, including the unarmed ones. The provers
//! run on dedicated OS threads that are deliberately not cancellable and hold
//! the round lock through a cloned `Arc`, so a drive that ends early can leave
//! a thread still writing to the sidecar. In the parent that would corrupt the
//! state a test is about to read; in a child it dies with the process.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use zcash_voting::{
    delegate::{gather_delegation_lwd_inputs, ResolveDelegationLwdParams},
    delegation_pipeline::{DelegationPipeline, SqliteWalletDbOpener},
    round::VotingDb,
    round_drive::{
        FailureIsolation, RoundDriveEvent, RoundDrivePolicy, RoundDriveReporter,
        RoundDriveReporterBridge, RoundDriver, RoundHostSource,
    },
    session::Decision,
    share_policy::ShareTimingPolicy,
    BallotIntent, ChainAdvancePolicy, ChainSubmissionClientConfig, ChainSubmissionControl,
    DelegationSigner, DelegationStepInputs, HelperClient, HelperHealth, HyperTransport, PirFleet,
    ProposalRosterEntry, RoundBinding, RoundExecutor, RoundHostContext, RoundStepProgress,
    ShareTrackingDrivePolicy, ShareTrackingDriver, ShareTrackingEvent, ShareTrackingHostContext,
    ShareTrackingHostSourceBridge, ShareTrackingQuiescence, ShareTrackingReporterBridge,
};

use crate::child::{CrashHelperTransport, CrashLog, CrashReporter, CrashTarget, CrashTransport};
use crate::helper_fleet::HelperFleetRoute;
use crate::stall::{RequestClassifier, StallTarget, StallingRoute};

/// The route every transport in a run shares.
///
/// Named once because the type is a stack rather than a word, and because
/// every transport must be built on the *same* value: the fleet's availability
/// and the armed stall have to apply to helper, chain, PIR, and tree traffic
/// alike, and two separately constructed routes would silently disagree.
type SuiteRoute = StallingRoute<HelperFleetRoute<zcash_voting::transport::DirectRoute>>;
use crate::environment::ZCASH_NETWORK;
use crate::provisioning::fetch_round;
use crate::run_config::{
    Endpoints, FailureRecord, RoundRunConfig, RunOutcome, ShareDeliveryRecord,
    ShareTrackingSummary, Target,
};
use crate::signing;
use crate::stages::CrashStage;

/// Drives the round described by `config` and returns what it did.
///
/// An armed run never returns: it aborts inside the driver when its stage is
/// reached. A run that returns from an armed configuration therefore means the
/// stage was never reached, which the caller must treat as a failure rather
/// than a pass.
pub async fn drive_round(config: &RoundRunConfig) -> Result<RunOutcome> {
    let log = Arc::new(CrashLog::create(&config.crash_log)?);

    // Round parameters come from the chain that created the round, not from a
    // signed config. The suite provisioned this round itself minutes earlier,
    // so authenticating a document about it would exercise the config layer
    // rather than recovery. What is not skipped is agreement: reading the
    // chain's own record means a provisioning mistake surfaces as a mismatch.
    let round = fetch_round(&config.endpoints.chain_rpc, &config.round_id)?;
    anyhow::ensure!(
        round.is_active(),
        "round {} is not active; its ceremony has not confirmed",
        config.round_id
    );

    let database = Arc::new(VotingDb::open_path(&config.sidecar).map_err(voting_error)?);
    // The sidecar's wallet scope *is* the account UUID: note selection parses
    // it as one, so any other label fails deep inside selection with a UUID
    // parse error rather than at this boundary.
    database.set_wallet_id(&config.account_uuid);
    database
        .ensure_round(ZCASH_NETWORK, &round.params, None)
        .map_err(voting_error)?;

    // Setup runs only for a round this sidecar has not yet laid out. Repeating
    // it on a resumed round asks the store to move the round phase *backwards*
    // — `ensure_bundles` requests the bundles-ready phase while the round has
    // already advanced past it — and the store rightly refuses with
    // `refusing to regress round phase`. Resuming means continuing from durable
    // state, not rebuilding it.
    let existing_bundles = database
        .get_bundle_count(&config.round_id)
        .map_err(voting_error)?;
    if existing_bundles > 0 {
        anyhow::ensure!(
            existing_bundles as usize == crate::provisioning::EXPECTED_BUNDLE_COUNT,
            "resumed conformance round has an unexpected bundle count: {existing_bundles}"
        );
        eprintln!("run: resuming a round with {existing_bundles} bundles; setup not repeated");
        return drive(config, database, round, log).await;
    }

    let selected = zcash_voting::selection::select_notes_with_lwd(
        &database,
        config
            .wallet_db
            .to_str()
            .context("wallet path is not UTF-8")?,
        &config.endpoints.lightwalletd,
        ZCASH_NETWORK,
        round.params.snapshot_height,
    )
    .await
    .map_err(voting_error)?;
    anyhow::ensure!(
        selected.notes.len() == crate::provisioning::EXPECTED_VOTER_NOTES,
        "conformance voter must hold {} eligible notes, found {}",
        crate::provisioning::EXPECTED_VOTER_NOTES,
        selected.notes.len()
    );
    let layout = database
        .ensure_bundles(&config.round_id, &selected.voting_note_infos())
        .map_err(voting_error)?;
    anyhow::ensure!(
        layout.bundle_count as usize == crate::provisioning::EXPECTED_BUNDLE_COUNT,
        "conformance voter must produce {} bundles, found {}",
        crate::provisioning::EXPECTED_BUNDLE_COUNT,
        layout.bundle_count
    );
    eprintln!(
        "run: {} notes -> {} bundles",
        selected.notes.len(),
        layout.bundle_count
    );

    drive(config, database, round, log).await
}

/// Builds the executor and drives the round.
///
/// Split from setup so a resumed run reaches it directly: continuing from
/// durable state must not rebuild the layout that state was derived from.
async fn drive(
    config: &RoundRunConfig,
    database: Arc<VotingDb>,
    round: crate::provisioning::ChainRound,
    log: Arc<CrashLog>,
) -> Result<RunOutcome> {
    // Seed after layout on both fresh and resumed runs. Existing padding is
    // never overwritten; the PIR fault omits proofs but retains stable dummies.
    if let Some(template) = &config.warm_pir_from {
        let proof_cache = if config.stall.target == Some(crate::stall::StallTarget::PirQuery) {
            crate::precompute::ProofCacheSeed::Cold
        } else {
            crate::precompute::ProofCacheSeed::Warm
        };
        match crate::precompute::seed_precompute(
            &config.sidecar,
            template,
            &config.round_id,
            proof_cache,
        ) {
            Ok(seeded) => eprintln!(
                "run: seeded {} PIR proofs and {} padded-slot secret sets",
                seeded.proofs, seeded.padded_bundles
            ),
            // Not fatal: a cold run still works, it is only slower and more
            // exposed to a stalled endpoint.
            Err(error) => eprintln!("run: precompute not seeded: {error}"),
        }
    }

    let target_recovery = config.mode == crate::run_config::RunMode::RecoverCombined;
    let stop_after_share_outcome = config.mode == crate::run_config::RunMode::ObserveHelperOutage
        || config.stall.armed_target() == Some(StallTarget::SharePost);
    let signing_inputs = if target_recovery {
        None
    } else {
        let seed = signing::voter_seed()?;
        let hotkey =
            signing::voting_hotkey(&seed, &config.account_uuid, &config.round_id, ZCASH_NETWORK)?;

        let lwd = gather_delegation_lwd_inputs(ResolveDelegationLwdParams {
            lightwalletd_url: &config.endpoints.lightwalletd,
            network: ZCASH_NETWORK,
            round_params: round.params.clone(),
            round_name: "recovery-conformance",
        })
        .await
        .map_err(voting_error)?;

        let pipeline = DelegationPipeline::new(
            Arc::clone(&database),
            SqliteWalletDbOpener::new(
                config
                    .wallet_db
                    .to_str()
                    .context("wallet path is not UTF-8")?
                    .to_string(),
                ZCASH_NETWORK,
            ),
            lwd,
            &config.account_uuid,
            Some(hotkey.clone()),
            zcash_voting::recoverable_bundle_policy_v1(),
            None,
        )
        .map_err(voting_error)?;

        Some((seed, hotkey, pipeline))
    };

    let armed = config.armed_stage();
    // The crash seams wrap the same transports a host would use, so the
    // requests that reach staging are real. An unarmed run passes `None` and
    // the wrappers become pass-throughs, which keeps the control run on the
    // same code path rather than a parallel one.
    // Two wrappers under the SDK, each doing one thing. The fleet wrapper
    // decides which helpers exist; the stall wrapper decides which request
    // class stops answering. Both sit *below* every deadline the SDK applies,
    // which is what makes a bound the SDK claims into a bound this suite can
    // watch it keep. With an empty plan each is a pass-through, so a control
    // run and a faulted run share one code path.
    let route = Arc::new(StallingRoute::new(
        HelperFleetRoute::new(
            zcash_voting::transport::DirectRoute::default(),
            config.fleet.clone(),
            Arc::clone(&log),
        ),
        config.stall.clone(),
        RequestClassifier::new(config.endpoints.pir_urls.clone()),
        Arc::clone(&log),
    ));
    let route_for_helpers = Arc::clone(&route);
    let helper_client = HelperClient::new(
        Arc::new(CrashHelperTransport::new(
            HyperTransport::with_shared_route(Arc::clone(&route)),
            armed,
            Arc::clone(&log),
        )),
        HelperHealth::default(),
    );
    let chain_transport = CrashTransport::new(
        HyperTransport::with_shared_route(Arc::clone(&route)),
        armed,
        Arc::clone(&log),
    );

    let chain_stall = matches!(
        config.stall.armed_target(),
        Some(
            StallTarget::DelegationPost
                | StallTarget::VotePost
                | StallTarget::DelegateAndCastPost
                | StallTarget::TransactionLookup
                | StallTarget::CommitmentTreeRead
        )
    );
    let mut chain_config = ChainSubmissionClientConfig::for_network(
        ZCASH_NETWORK,
        config.endpoints.vote_servers.clone(),
    )
    .with_vote_chain_id(crate::environment::STAGING_CHAIN_ID);
    if chain_stall {
        // Exercise the full request deadline once. Repeating a permanently
        // hung POST across endpoints adds minutes without another assertion.
        // The subsequent unarmed recovery restores the default retry budget.
        chain_config = chain_config.with_post_attempts(1, Vec::new());
    }
    let executor = RoundExecutor::with_transport(
        Arc::clone(&database),
        chain_transport,
        chain_config,
        helper_client,
    )
    .map_err(|error| anyhow::anyhow!("building the executor: {error:?}"))?
    // Vote-tree reads default to a *fresh* `HyperTransport` over its own
    // route, so without this they escape both fault wrappers entirely — and
    // silently, because the round still works. The stall matrix caught it: the
    // `commitment-tree-read` target reported that no request of its class was
    // ever seen, when in fact the requests were being made somewhere the
    // wrapper could not observe. The same gap hid tree traffic from the helper
    // fleet wrapper.
    .with_tree_transport(Arc::new(HyperTransport::with_shared_route(Arc::clone(
        &route,
    ))))
    .with_binding(RoundBinding {
        round_id: config.round_id.clone(),
        network: ZCASH_NETWORK,
        proposals: roster(),
        hotkey_secret: signing_inputs
            .as_ref()
            .map(|(_, hotkey, _)| zeroize::Zeroizing::new(hotkey.stored_secret().to_vec())),
    })
    .map_err(voting_error)?;

    if !target_recovery {
        executor
            .set_ballot_intents(&ballot())
            .map_err(voting_error)?;
    } else {
        let plan = executor.plan().map_err(voting_error)?;
        anyhow::ensure!(
            matches!(plan.next_steps.first(), Some(zcash_voting::session::NextStep::AdvanceVoteBatch { bundle_index, proposal_id }) if *bundle_index == config.target.bundle_index && *proposal_id == config.target.proposal_id),
            "signerless run must select the persisted target batch first"
        );
    }

    let pir = Arc::new(
        PirFleet::new(
            &config.endpoints.pir_urls,
            pir_layout(),
            Arc::new(HyperTransport::with_shared_route(route)),
        )
        .map_err(voting_error)?,
    );

    let host = Host {
        helper_urls: config.endpoints.helper_urls.clone(),
        vote_tree_urls: config.endpoints.vote_servers.clone(),
        delegation: signing_inputs.map(|(seed, _, pipeline)| DelegationStepInputs {
            driver: Arc::new(pipeline),
            signer: DelegationSigner::Software(signing::software_signer(seed)),
            pir,
        }),
        chain_policy: chain_policy_for(armed, chain_stall),
    };

    let crash_reporter = CrashReporter::new(
        armed,
        CrashTarget {
            bundle_index: config.target.bundle_index,
            proposal_id: config.target.proposal_id,
        },
        Arc::clone(&log),
    );
    // Before the drive, not after. A crash around a share POST leaves one
    // helper journaled with an unknown outcome, and only this pass re-sends it.
    // Until it does, that share has no accepted helper, which keeps
    // `blocking_recovery` true, which stops the driver from ever quiescing at
    // `BackgroundShareWorkOnly` — so a recovery pass placed after the drive is
    // unreachable on precisely the runs that need it. The driver polls the
    // abandoned share until its budget runs out instead.
    let mut share_tracking = Vec::new();
    if armed.is_none() && !target_recovery && !stop_after_share_outcome {
        match recover_pending_shares(&database, config, &route_for_helpers).await {
            Ok(summary) => share_tracking.push(summary),
            Err(error) => eprintln!("run: background share tracking stopped early: {error}"),
        }
    }

    let control = ChainSubmissionControl::new(0);
    let reporter = RoundDriveReporterBridge::new(|event| {
        // Refused and timed-out deliveries stay Pending, so the normal driver
        // deliberately retries them. Outage and share-POST stall fixtures need
        // one completed delivery observation before restoring networking.
        // Host cancellation preserves the SDK's completed durable effects.
        if stop_after_share_outcome
            && matches!(
                &event,
                RoundDriveEvent::StepProgress {
                    progress: RoundStepProgress::ShareOutcome(_),
                    ..
                }
            )
        {
            control.cancel();
        }
        crash_reporter.report(event);
    });
    let report = RoundDriver::new(&executor)
        .with_policy(policy(
            armed.is_some() || target_recovery,
            if target_recovery {
                1
            } else {
                config.max_dispatches
            },
        ))
        .run(&host, &control, &reporter)
        .await;

    // Again afterwards: the drive itself can leave a share placed but
    // unconfirmed, and the round is not settled until tracking says so.
    if armed.is_none() && !target_recovery && !stop_after_share_outcome {
        match recover_pending_shares(&database, config, &route_for_helpers).await {
            Ok(summary) => share_tracking.push(summary),
            Err(error) => eprintln!("run: background share tracking stopped early: {error}"),
        }
    }

    let outcome = RunOutcome {
        quiescence: format!("{:?}", report.quiescence),
        quiescence_kind: quiescence_kind(&report.quiescence),
        failures: report
            .failures
            .iter()
            .map(|failure| FailureRecord {
                step: failure.step.as_ref().map(|step| format!("{step:?}")),
                bundle_index: failure.bundle_index,
                kind: format!("{:?}", failure.failure.kind),
                message: failure.failure.message.clone(),
            })
            .collect(),
        dispatches: 0,
        share_tracking,
    };
    let mut outcome = outcome;
    if target_recovery {
        let snapshot = crate::assertions::DurableSnapshot::read(&config.sidecar)?;
        let bundle = snapshot
            .combined
            .iter()
            .find(|b| {
                b.bundle_index == config.target.bundle_index
                    && b.round_id == config.round_id
                    && b.wallet_id == config.account_uuid
            })
            .context("signerless target disappeared")?;
        outcome.quiescence_kind = if bundle.assert_confirmed(&proposal_ids()).is_ok() {
            "TargetRecovered"
        } else {
            "TargetRecoveryPending"
        }
        .to_string();
        outcome.quiescence = outcome.quiescence_kind.clone();
    }
    outcome.write(&config.outcome)?;
    Ok(outcome)
}

/// Pacing and isolation for one run.
///
/// An armed run is serialised to one bundle. `CrashTransport` sees an HTTP
/// request, not a bundle index, so with bundles running concurrently the crash
/// lands on whichever wins the race and a test naming one bundle silently
/// asserts against another. `StopRound` for the same reason: under
/// `SkipBundle`, a transport failure on the target bundle moves the driver on
/// to the next, which then reaches the broadcast and fires the stage on a
/// bundle the test did not name.
///
/// An unarmed run keeps the shipped failure isolation, because that is the
/// behaviour a host actually gets and the control must reflect it. It does not
/// keep the default concurrency: staging serves PIR from a single endpoint that
/// performs the query synchronously, so three bundles fetching five proofs each
/// puts fifteen concurrent CPU-heavy queries on one server. Under that load it
/// stops answering — refusing connections, not merely slowing — and the run
/// fails for a reason that has nothing to do with recovery. One bundle at a
/// time is the smaller lever than any client-side retry, which only resends
/// work the server may still be computing.
fn policy(armed: bool, max_dispatches: usize) -> RoundDrivePolicy {
    let base = RoundDrivePolicy {
        max_bundle_concurrency: std::num::NonZeroUsize::new(1).expect("1 is not zero"),
        max_dispatches,
        ..RoundDrivePolicy::default()
    };
    if !armed {
        return base;
    }
    // An armed run additionally stops at the first failure. Under `SkipBundle`
    // a transport failure on the target bundle moves the driver to the next,
    // which then reaches the broadcast and fires the stage on a bundle the test
    // did not name.
    RoundDrivePolicy {
        failure_isolation: FailureIsolation::StopRound,
        ..base
    }
}

/// Names the quiescence variant without depending on its `Debug` shape.
fn quiescence_kind(quiescence: &zcash_voting::round_drive::RoundQuiescence) -> String {
    use zcash_voting::round_drive::RoundQuiescence as Q;
    match quiescence {
        Q::NoWorkLeft => "NoWorkLeft",
        Q::NeedsBallot { .. } => "NeedsBallot",
        Q::NeedsDelegationSignatures { .. } => "NeedsDelegationSignatures",
        Q::BackgroundShareWorkOnly { .. } => "BackgroundShareWorkOnly",
        Q::Cancelled => "Cancelled",
        Q::ChainTerminal { .. } => "ChainTerminal",
        Q::ChainRecoveryStalled { .. } => "ChainRecoveryStalled",
        Q::Failures => "Failures",
        Q::PassBudgetExhausted { .. } => "PassBudgetExhausted",
        _ => "Unknown",
    }
    .to_string()
}

/// Supplies the host context fresh for every dispatch.
///
/// Read once per dispatch rather than once per run: a proof can take minutes
/// and cross the last-moment boundary, so the step that follows plans against
/// the clock it actually runs under.
struct Host {
    helper_urls: Vec<String>,
    vote_tree_urls: Vec<String>,
    delegation: Option<DelegationStepInputs>,
    chain_policy: ChainAdvancePolicy,
}

/// The chain policy one run drives under.
///
/// `ChainOutcome` is reported once per `advance_step`, at the end, carrying the
/// episode's *terminal* outcome — not once per poll. With the default 45
/// passes an episode polls until the submission confirms, so that outcome is
/// effectively always `Confirmed` and a stage waiting to see a submission
/// *still tracking* can never fire.
///
/// Capping the episode at one pass is what makes the tracking window
/// observable: the episode ends while the submission is still pending and
/// reports it. This is ordinary host configuration, not a test hook — a host
/// that wants to interleave work uses a short policy for exactly this reason —
/// and it also bounds chain stall episodes to one pass through the full SDK
/// request deadline. Unarmed recovery keeps the shipped cadence.
fn chain_policy_for(armed: Option<CrashStage>, chain_stall: bool) -> ChainAdvancePolicy {
    if armed == Some(CrashStage::AfterTracking) || chain_stall {
        return ChainAdvancePolicy {
            max_passes: 1,
            ..ChainAdvancePolicy::default()
        };
    }
    ChainAdvancePolicy::default()
}

impl RoundHostSource for Host {
    fn host_context(&self) -> RoundHostContext {
        RoundHostContext {
            configured_helper_urls: self.helper_urls.clone(),
            now_seconds: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or_default(),
            ceremony_start_seconds: None,
            vote_end_time_seconds: None,
            vote_tree_node_urls: self.vote_tree_urls.clone(),
            delegation: self.delegation.clone(),
            chain_policy: self.chain_policy.clone(),
            max_proof_concurrency: 1,
        }
    }
}

/// How long an interrupted share attempt waits before it may be retried.
///
/// Both bounds of the policy are set to this, so the threshold is exactly this
/// value rather than a quarter of the remaining round window.
/// Long enough that a retry is a considered recovery rather than an immediate
/// double-send, short enough that a test can watch it happen.
const SHARE_RETRY_AFTER_SECONDS: u64 = 45;

/// Wall-clock ceiling on background tracking for one run.
///
/// The driver itself has no wall bound, and correctly so: a healthy run ends at
/// the round's vote end, which for these rounds is one hour after creation. A
/// conformance run needs an answer inside a test budget, so the run is raced
/// against this instead. Dropping the future is a supported way to end a run —
/// it releases the round's admission — so the race is not a leak.
const SHARE_TRACKING_BUDGET: Duration = Duration::from_secs(8 * 60);

/// Tracking passes before the run gives up, whatever the clock says.
///
/// `max_passes` is `None` by default because vote end is what ends a healthy
/// run. Here it gives a bounded pass count independently of the one-hour round
/// window, including when a share has no helper that will ever confirm it.
const MAX_TRACKING_PASSES: u32 = 60;

/// How long a failed pass waits before the next one is attempted.
///
/// Shorter than the shipped fifteen seconds because a conformance run is
/// racing [`SHARE_TRACKING_BUDGET`] rather than a voting window.
const TRACKING_FAILURE_RETRY: Duration = Duration::from_secs(5);

/// Consecutive failed passes before tracking reports `Failing`.
///
/// Far below the shipped 240. That default is sized to ride out a helper
/// outage across an hour because nothing would restart the run; here a failing
/// fleet is a finding the matrix should be told about promptly, and the run is
/// bounded anyway.
const MAX_CONSECUTIVE_TRACKING_FAILURES: u32 = 6;

/// Runs the host's own background share tracking until nothing is pending.
///
/// The round driver deliberately stops at `BackgroundShareWorkOnly`: an
/// unconfirmed share that some helper already accepted is the host's timer to
/// finish, not the foreground run's. The `ConfirmShare` step it does dispatch
/// only *polls* status — `confirm_pending_share` never re-POSTs. So the one
/// component that recovers an interrupted attempt was never running anywhere
/// in this suite.
///
/// That is why the share crash stages could not converge. A crash around a
/// share POST leaves exactly one helper journaled as attempted with its outcome
/// unknown, and nothing was ever going to re-send it: the run delivered every
/// other share and then sat polling the one it had abandoned. The D1..D5
/// invariants describe recovery this suite was not performing.
///
/// # Why the driver rather than a loop of passes
///
/// This used to assemble its own loop out of `track_pending_shares`,
/// `next_delay_seconds`, and a pass counter. `ShareTrackingDriver` now owns
/// that cadence, and the pieces the loop was built from are no longer a
/// supported way to do anything — they were a second, unenforced schedule
/// beside the one the driver applies. A conformance suite asserting recovery
/// behaviour must drive shares the way a host actually does, or it is
/// asserting against a schedule no host will ever run.
///
/// The suite still supplies the two things a host legitimately owns: the
/// timing thresholds, and the bounds that stop a run. Nothing here changes
/// *whether* a retry is duplicate-safe, which is the property the share stages
/// exist to test.
///
/// # Timing
///
/// A share becomes retryable after a quarter of the remaining vote window,
/// bounded by the policy. With a one-hour round, the default can delay a lost
/// POST for many minutes. The explicit threshold makes recovery observable
/// within the case budget.
///
/// `max_overdue_threshold_seconds` is therefore lowered here. It changes *when*
/// a retry becomes allowed, not whether the retry is duplicate-safe. Supplying
/// a timing policy is ordinary host configuration; `ShareTimingPolicy` is
/// public for exactly this.
async fn recover_pending_shares(
    database: &Arc<VotingDb>,
    config: &RoundRunConfig,
    route: &Arc<SuiteRoute>,
) -> Result<ShareTrackingSummary> {
    let client = HelperClient::new(
        Arc::new(HyperTransport::with_shared_route(Arc::clone(route))),
        HelperHealth::default(),
    );
    let policy = ShareTrackingDrivePolicy {
        timing: ShareTimingPolicy {
            max_overdue_threshold_seconds: SHARE_RETRY_AFTER_SECONDS,
            min_overdue_threshold_seconds: SHARE_RETRY_AFTER_SECONDS,
            ..ShareTimingPolicy::default()
        },
        failure_retry: TRACKING_FAILURE_RETRY,
        max_consecutive_failures: MAX_CONSECUTIVE_TRACKING_FAILURES,
        max_passes: Some(MAX_TRACKING_PASSES),
    };

    // Read once per pass, not once per run: the fleet a pass runs against is
    // the fleet at the moment it starts, which is the seam a changing helper
    // fleet is observed through.
    let host = ShareTrackingHostSourceBridge::new(|| ShareTrackingHostContext {
        configured_helper_urls: config.endpoints.helper_urls.clone(),
        now_seconds: now_seconds(),
        vote_end_time_seconds: Some(config.vote_end_time_seconds),
    });
    let reporter = ShareTrackingReporterBridge::new(|event: ShareTrackingEvent| {
        // Only passes that did something are printed. A quiet pass per
        // fifteen-second poll over a multi-minute budget would bury the ones
        // that matter.
        match event {
            ShareTrackingEvent::PassFinished { pass, report } => {
                if !report.confirmed.is_empty()
                    || !report.resubmitted.is_empty()
                    || !report.ambiguous.is_empty()
                {
                    eprintln!(
                        "run: tracking pass {pass}: {} confirmed, {} resubmitted, {} ambiguous",
                        report.confirmed.len(),
                        report.resubmitted.len(),
                        report.ambiguous.len()
                    );
                }
            }
            ShareTrackingEvent::PassFailed { pass, message, .. } => {
                eprintln!("run: tracking pass {pass} failed: {message}");
            }
            _ => {}
        }
    });
    let control = ChainSubmissionControl::new(0);

    let driver = ShareTrackingDriver::new(database, &client, &config.round_id).with_policy(policy);
    let Ok(report) = tokio::time::timeout(
        SHARE_TRACKING_BUDGET,
        driver.run(&host, &control, &reporter),
    )
    .await
    else {
        // Not an error: the budget is the suite's, not the round's, and the
        // durable state a timed-out run leaves is exactly what the assertions
        // read. Dropping the future released the round.
        eprintln!("run: background share tracking hit its {SHARE_TRACKING_BUDGET:?} budget");
        // The budget is the suite's, not the round's. What tracking managed
        // before it expired is real and durable; there is simply no report to
        // describe it, which the empty summary says plainly.
        return Ok(ShareTrackingSummary {
            quiescence: "SuiteBudgetExpired".to_string(),
            ..ShareTrackingSummary::default()
        });
    };

    eprintln!(
        "run: share tracking ended at {:?} after {} passes: {} confirmed, {} resubmitted, \
         {} ambiguous, {} unrecoverable",
        report.quiescence,
        report.passes,
        report.confirmed.len(),
        report.resubmitted.len(),
        report.ambiguous.len(),
        report.unrecoverable.len(),
    );
    if let ShareTrackingQuiescence::Failing { messages } = &report.quiescence {
        // Surfaced rather than returned. A failing fleet leaves durable state
        // the assertions still need to read, and the matrix judges the round
        // from that state rather than from this run's mood.
        for message in messages {
            eprintln!("run: tracking failure: {message}");
        }
    }

    Ok(ShareTrackingSummary {
        quiescence: format!("{:?}", report.quiescence),
        passes: report.passes,
        confirmed: report.confirmed.len(),
        resubmitted: report.resubmitted.iter().map(delivery_record).collect(),
        ambiguous: report.ambiguous.iter().map(delivery_record).collect(),
        unrecoverable: report.unrecoverable.len(),
    })
}

/// Flattens one `(share, helper)` pair for the parent.
fn delivery_record(
    resubmitted: &zcash_voting::share_tracking::ResubmittedShare,
) -> ShareDeliveryRecord {
    ShareDeliveryRecord {
        bundle_index: resubmitted.share.bundle_index,
        proposal_id: resubmitted.share.proposal_id,
        share_index: resubmitted.share.share_index,
        server_url: resubmitted.server_url.clone(),
    }
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// The roster the round was provisioned with.
pub fn roster() -> Vec<ProposalRosterEntry> {
    crate::provisioning::suite_ballot()
        .iter()
        .map(|proposal| ProposalRosterEntry {
            proposal_id: proposal.id,
            num_options: u32::try_from(proposal.options.len()).unwrap_or_default(),
        })
        .collect()
}

/// A terminal decision for every proposal, so no cast is gated on the ballot.
///
/// Derived from the same ballot the round was provisioned with, so a proposal
/// added there cannot be silently left undecided here.
pub fn ballot() -> Vec<BallotIntent> {
    crate::provisioning::suite_ballot()
        .iter()
        .enumerate()
        .map(|(index, proposal)| BallotIntent {
            proposal_id: proposal.id,
            decision: Decision::Choice(u32::try_from(index).unwrap_or_default()),
        })
        .collect()
}

/// Proposal ids in the round, for `resume_plan`.
pub fn proposal_ids() -> Vec<u32> {
    crate::provisioning::suite_ballot()
        .iter()
        .map(|proposal| proposal.id)
        .collect()
}

/// The PIR layout the staging deployment publishes.
pub fn pir_layout() -> zcash_voting::config::PirLayout {
    zcash_voting::config::PirLayout {
        pir_depth: 19,
        tier0_layers: 12,
        tier1_layers: 7,
        poly_len: 4096,
    }
}

fn voting_error(error: zcash_voting::VotingError) -> anyhow::Error {
    anyhow::anyhow!("{error:?}")
}

/// The endpoint that really answers share submissions.
///
/// Helpers are not PIR. The share endpoint lives on the vote server
/// (`/shielded-vote/v1/shares`), and only the primary answers it on staging —
/// the secondary and the PIR host both return 404. This is therefore both the
/// suite's single real helper and the backend every synthetic helper is routed
/// to.
pub fn helper_backend(deployment: &crate::stage_config::StageDeployment) -> String {
    deployment
        .vote_server_urls()
        .first()
        .cloned()
        .unwrap_or_else(|| crate::environment::STAGING_VOTE_SERVER_FALLBACK.to_string())
}

/// Endpoints whose helper fleet is `fleet`'s synthetic one.
///
/// The configured fleet includes helpers that are down: a helper that refuses
/// a connection is still configured, and the SDK draws a real distinction
/// between an unreachable helper and one removed from the fleet. Reporting only
/// the reachable ones would quietly turn every outage scenario into a fleet
/// contraction.
pub fn endpoints_with_fleet(
    deployment: &crate::stage_config::StageDeployment,
    fleet: &crate::helper_fleet::HelperFleetPlan,
) -> Endpoints {
    let mut endpoints = endpoints_from(deployment);
    let configured = fleet.configured_urls();
    if !configured.is_empty() {
        endpoints.helper_urls = configured;
    }
    endpoints
}

/// Builds the endpoint set from the published staging deployment.
pub fn endpoints_from(deployment: &crate::stage_config::StageDeployment) -> Endpoints {
    Endpoints {
        chain_rpc: crate::environment::STAGING_CHAIN_RPC.to_string(),
        vote_servers: deployment.vote_server_urls(),
        pir_urls: deployment.pir_urls(),
        // Helpers are not PIR. The share endpoint lives on the vote server
        // (`/shielded-vote/v1/shares`), and only the primary answers it on
        // staging — the secondary and the PIR host both return 404. Pointing
        // share delivery at PIR fails as `HelperDeliveryIncomplete`, which
        // reads like a delivery defect rather than a misconfiguration.
        helper_urls: deployment.vote_server_urls().into_iter().take(1).collect(),
        lightwalletd: crate::environment::LIGHTWALLETD_URLS[0].to_string(),
    }
}

/// The default target: the first bundle and the first proposal.
pub fn default_target() -> Target {
    Target {
        bundle_index: 0,
        proposal_id: crate::provisioning::suite_ballot()
            .first()
            .map(|proposal| proposal.id)
            .unwrap_or(1),
    }
}
