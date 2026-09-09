//! Building and driving one benchmark round, in the child process.
//!
//! This is the sibling of `recovery-conformance/src/round_run.rs`, and the two
//! deliberately stay separate rather than sharing one parameterised driver.
//! That file is the *control* three live fault matrices compare against;
//! threading a benchmark's needs through it would change the thing those
//! matrices are measured by. What this drops from it: the crash seams, the
//! stall route, `RunMode`, and the fixed three-proposal ballot. What it adds:
//! reported entry points, so every phase of the run is timed.
//!
//! Every drive runs in a child process, as it does there, and for the same
//! reason: the provers run on dedicated OS threads that are not cancellable and
//! hold the round lock through a cloned `Arc`, so a drive that ends early can
//! leave a thread still writing to the sidecar. In a child that dies with the
//! process; in the parent it would corrupt the state the metrics are read from.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use recovery_conformance::child::CrashLog;
use recovery_conformance::environment::ZCASH_NETWORK;
use recovery_conformance::helper_fleet::HelperFleetRoute;
use recovery_conformance::provisioning::fetch_round;
use recovery_conformance::round_run::pir_layout;
use recovery_conformance::signing;
use zcash_voting::{
    delegate::{gather_delegation_lwd_inputs, ResolveDelegationLwdParams},
    delegation_pipeline::{DelegationPipeline, SqliteWalletDbOpener},
    round::VotingDb,
    round_drive::{
        FailureIsolation, RoundDriveEvent, RoundDrivePolicy, RoundDriveReporterBridge, RoundDriver,
        RoundHostSource,
    },
    share_policy::ShareTimingPolicy,
    ChainAdvancePolicy, ChainSubmissionClientConfig, ChainSubmissionControl, DelegationSigner,
    DelegationStepInputs, HelperClient, HelperHealth, HyperTransport, ObservabilityOptions,
    OperationObservability, PirFleet, RoundBinding, RoundExecutor, RoundHostContext,
    ShareTrackingDrivePolicy, ShareTrackingDriver, ShareTrackingEvent, ShareTrackingHostContext,
    ShareTrackingHostSourceBridge, ShareTrackingReporterBridge,
};

use crate::events::{EventLog, PhaseEvent};
use crate::run_config::{BenchOutcome, BenchRunConfig, FailureRecord, TrackingSummary};

/// The route every transport in a run shares.
///
/// One value, not one per transport. The helper fleet has to apply to helper,
/// chain, PIR, **and** vote-tree traffic alike, and two separately constructed
/// routes would silently disagree about which helpers exist.
type BenchRoute = HelperFleetRoute<zcash_voting::transport::DirectRoute>;

/// Background tracking passes before the run gives up, whatever the clock says.
///
/// The shipped default is `None`, because a healthy host is ended by the
/// round's vote end. A benchmark needs an answer inside its own budget.
const MAX_TRACKING_PASSES: u32 = 40;

/// Drives the round described by `config` and returns what it did.
///
/// Writes every observability snapshot, the phase-event log, and the outcome
/// into the run directory as it goes, so a run that fails part way still leaves
/// the diagnostics that explain where it stopped.
pub async fn drive(config: &BenchRunConfig) -> Result<BenchOutcome> {
    let events = EventLog::create(&config.run_dir)?;
    let contacts = Arc::new(CrashLog::create(
        config.run_dir.join("helper-contacts.jsonl").as_path(),
    )?);

    // Round parameters come from the chain that created the round. The parent
    // provisioned it minutes earlier, so reading the chain's own record means a
    // provisioning mistake surfaces here as a mismatch rather than as an
    // unexplained failure deep inside the drive.
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

    events.record(PhaseEvent::phase("setup::select_notes"));
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
    let layout = database
        .ensure_bundles(&config.round_id, &selected.voting_note_infos())
        .map_err(voting_error)?;
    // Observed and reported rather than asserted. A wallet rebalance changes
    // the workload this benchmark measures; it does not invalidate the run, and
    // failing here would make an unrelated funding change look like a defect.
    eprintln!(
        "bench: {} notes -> {} bundles, {} proposals",
        selected.notes.len(),
        layout.bundle_count,
        config.ballot.len()
    );
    events.record(PhaseEvent::phase("setup::bundles_ready"));

    // After the layout, never before it. The template's padded-slot secrets are
    // copied onto this round's `bundles` rows, and those rows do not exist until
    // `ensure_bundles` writes them — so seeding earlier silently copies nothing,
    // every padded slot then misses the proof cache, and the run refetches all of
    // them from the one synchronous PIR endpoint.
    seed_precompute(config, &events);

    let seed = signing::voter_seed()?;
    let hotkey =
        signing::voting_hotkey(&seed, &config.account_uuid, &config.round_id, ZCASH_NETWORK)?;

    let lwd = gather_delegation_lwd_inputs(ResolveDelegationLwdParams {
        lightwalletd_url: &config.endpoints.lightwalletd,
        network: ZCASH_NETWORK,
        round_params: round.params.clone(),
        round_name: "stage-bench",
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

    // With an empty fleet plan this wrapper delegates every request unchanged,
    // so a one-helper run and a ten-helper run share one code path.
    let route = Arc::new(HelperFleetRoute::new(
        zcash_voting::transport::DirectRoute::default(),
        config.fleet.clone(),
        Arc::clone(&contacts),
    ));

    let helper_client = HelperClient::new(
        Arc::new(HyperTransport::with_shared_route(Arc::clone(&route))),
        HelperHealth::default(),
    );
    let chain_config = ChainSubmissionClientConfig::for_network(
        ZCASH_NETWORK,
        config.endpoints.vote_servers.clone(),
    )
    .with_vote_chain_id(recovery_conformance::environment::STAGING_CHAIN_ID);

    let executor = RoundExecutor::with_transport(
        Arc::clone(&database),
        HyperTransport::with_shared_route(Arc::clone(&route)),
        chain_config,
        helper_client,
    )
    .map_err(|error| anyhow::anyhow!("building the executor: {error:?}"))?
    // Vote-tree reads default to a *fresh* transport over their own route, so
    // without this they escape the fleet wrapper entirely — and silently,
    // because the round still works.
    .with_tree_transport(Arc::new(HyperTransport::with_shared_route(Arc::clone(
        &route,
    ))))
    .with_binding(RoundBinding {
        round_id: config.round_id.clone(),
        network: ZCASH_NETWORK,
        proposals: config.ballot.roster(),
        hotkey_secret: Some(zeroize::Zeroizing::new(hotkey.stored_secret().to_vec())),
    })
    .map_err(voting_error)?;

    executor
        .set_ballot_intents(&config.ballot.intents())
        .map_err(voting_error)?;

    let pir = Arc::new(
        PirFleet::new(
            &config.endpoints.pir_urls,
            pir_layout(),
            Arc::new(HyperTransport::with_shared_route(Arc::clone(&route))),
        )
        .map_err(voting_error)?,
    );

    let host = Host {
        helper_urls: config.endpoints.helper_urls.clone(),
        vote_tree_urls: config.endpoints.vote_servers.clone(),
        vote_end_time_seconds: config.vote_end_time_seconds,
        delegation: Some(DelegationStepInputs {
            driver: Arc::new(pipeline),
            signer: DelegationSigner::Software(signing::software_signer(seed)),
            pir,
        }),
    };

    let options = ObservabilityOptions {
        max_records: config.max_records,
        max_summary_groups: config.max_records,
        max_active_stages: config.max_records,
    };

    let control = ChainSubmissionControl::new(0);
    let reporter = RoundDriveReporterBridge::new(|event| events.record(round_event(&event)));

    events.record(PhaseEvent::phase("round::drive_started"));
    let started = Instant::now();
    let (report, snapshot) = RoundDriver::new(&executor)
        .with_policy(policy(config))
        .run_with_report(&host, &control, &reporter, Some(options))
        .await
        .into_parts();
    let round_drive_seconds = started.elapsed().as_secs_f64();
    events.record(PhaseEvent::phase("round::drive_finished"));
    // Announced here, not only in the final table. Delivery is the phase a host
    // actually waits on; everything after it is background work the product
    // spreads across the voting window, and a terminal that went quiet at this
    // point looked to a reader exactly like a hang.
    eprintln!(
        "bench: delivery finished in {round_drive_seconds:.1}s — {} of {} proposals, \
         {} shares placed. Confirmation follows and is background work.",
        report.tally.completed_proposals,
        report.tally.total_proposals,
        placed_shares(&database, &config.round_id),
    );
    // Written before anything can fail on the domain result: diagnostics that
    // survive only a successful run cannot explain an unsuccessful one.
    save_snapshot(&config.run_dir, "round.observability.json", snapshot);

    let mut tracking = Vec::new();
    let tracking_started = Instant::now();
    let budget = Duration::from_secs(config.tracking_budget_seconds);
    if config.confirm_concurrency > 1 {
        // The experiment, never beside the shipped path: a round admits one
        // run, and interleaving the two would double helper traffic for no
        // added progress.
        match confirm_shares_concurrently(&database, config, &route, &events, budget).await {
            Ok(run) => {
                save_snapshots(
                    &config.run_dir,
                    "confirm.observability.json",
                    &run.snapshots,
                );
                tracking.push(run.summary);
            }
            Err(error) => eprintln!("bench: concurrent confirmation stopped early: {error}"),
        }
    } else {
        match track_shares(&database, config, &route, &events, options, budget).await {
            Ok((summary, snapshot)) => {
                save_snapshot(&config.run_dir, "tracking.0.observability.json", snapshot);
                tracking.push(summary);
            }
            Err(error) => eprintln!("bench: background share tracking stopped early: {error}"),
        }
    }
    let tracking_seconds = tracking_started.elapsed().as_secs_f64();

    let outcome = BenchOutcome {
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
        notes: selected.notes.len(),
        bundles: layout.bundle_count,
        proposals: config.ballot.len(),
        completed_proposals: report.tally.completed_proposals as usize,
        tracking,
        round_drive_seconds,
        tracking_seconds,
    };
    outcome.write(&BenchOutcome::path_in(&config.run_dir))?;
    Ok(outcome)
}

/// Pacing for one benchmark run.
///
/// `max_bundle_concurrency` is a measured variable rather than a constant: the
/// staging PIR fleet is one endpoint that answers synchronously, so raising it
/// is a deliberate experiment about that endpoint, not free parallelism. The
/// default of one is what a warm, well-behaved run uses.
///
/// Failure isolation stays at the shipped `SkipBundle`, because that is the
/// behaviour a host actually gets and a benchmark that changed it would be
/// timing something no host runs.
fn policy(config: &BenchRunConfig) -> RoundDrivePolicy {
    RoundDrivePolicy {
        max_bundle_concurrency: std::num::NonZeroUsize::new(config.bundle_concurrency)
            .unwrap_or(std::num::NonZeroUsize::MIN),
        failure_isolation: FailureIsolation::SkipBundle,
        max_dispatches: config.max_dispatches,
        ..RoundDrivePolicy::default()
    }
}

/// Supplies the host context fresh for every dispatch.
///
/// Read once per dispatch rather than once per run: a proof can take minutes
/// and cross the last-moment boundary, so the step that follows plans against
/// the clock it actually runs under.
struct Host {
    helper_urls: Vec<String>,
    vote_tree_urls: Vec<String>,
    vote_end_time_seconds: u64,
    delegation: Option<DelegationStepInputs>,
}

impl RoundHostSource for Host {
    fn host_context(&self) -> RoundHostContext {
        RoundHostContext {
            configured_helper_urls: self.helper_urls.clone(),
            now_seconds: now_seconds(),
            ceremony_start_seconds: None,
            // Supplied, unlike the conformance suite's host: share scheduling
            // derives its overdue and last-moment windows from the distance to
            // vote end, and a benchmark that withheld it would measure a
            // schedule no real host runs.
            vote_end_time_seconds: Some(self.vote_end_time_seconds),
            vote_tree_node_urls: self.vote_tree_urls.clone(),
            delegation: self.delegation.clone(),
            chain_policy: ChainAdvancePolicy::default(),
            max_proof_concurrency: 1,
        }
    }
}

/// Runs the host's own background share tracking until nothing is pending.
///
/// The round driver deliberately stops at `BackgroundShareWorkOnly`: a share
/// some helper already accepted but has not yet confirmed is the host's timer
/// to finish. A benchmark that stopped there would report a delivery time
/// missing its confirmation tail.
///
/// The shipped timing policy is kept unchanged. The conformance suite lowers
/// its retry thresholds so a recovery is observable inside a case budget; here
/// that would measure a cadence no host applies.
async fn track_shares(
    database: &Arc<VotingDb>,
    config: &BenchRunConfig,
    route: &Arc<BenchRoute>,
    events: &EventLog,
    options: ObservabilityOptions,
    budget: Duration,
) -> Result<(TrackingSummary, Option<OperationObservability>)> {
    let client = HelperClient::new(
        Arc::new(HyperTransport::with_shared_route(Arc::clone(route))),
        HelperHealth::default(),
    );
    let policy = ShareTrackingDrivePolicy {
        timing: ShareTimingPolicy::default(),
        max_passes: Some(MAX_TRACKING_PASSES),
        ..ShareTrackingDrivePolicy::default()
    };
    // Read once per pass, not once per run: the fleet a pass runs against is
    // the fleet at the moment it starts.
    let host = ShareTrackingHostSourceBridge::new(|| ShareTrackingHostContext {
        configured_helper_urls: config.endpoints.helper_urls.clone(),
        now_seconds: now_seconds(),
        vote_end_time_seconds: Some(config.vote_end_time_seconds),
    });
    let reporter = ShareTrackingReporterBridge::new(|event: ShareTrackingEvent| {
        // Echoed as well as logged. A pass over a large round takes minutes and
        // the tracker emits nothing inside one, so a terminal with no output
        // here is indistinguishable from a wedged process.
        match &event {
            ShareTrackingEvent::PassFinished { pass, report } => eprintln!(
                "bench: tracking pass {pass}: {} confirmed, {} resubmitted, {} ambiguous",
                report.confirmed.len(),
                report.resubmitted.len(),
                report.ambiguous.len()
            ),
            ShareTrackingEvent::PassFailed { pass, message, .. } => {
                eprintln!("bench: tracking pass {pass} failed: {message}")
            }
            ShareTrackingEvent::AwaitingNextPass { delay, .. } => eprintln!(
                "bench: tracking waiting {}s for the next pass",
                delay.as_secs()
            ),
            _ => {}
        }
        events.record(tracking_event(&event));
    });
    let control = ChainSubmissionControl::new(0);

    events.record(PhaseEvent::phase("tracking::started"));
    let driver = ShareTrackingDriver::new(database, &client, &config.round_id).with_policy(policy);

    // Cancelled, not timed out. Racing the run against a `tokio::time::timeout`
    // drops its future, and a dropped invocation never freezes its report — so
    // the budget that bounds a large round is exactly the case whose
    // confirmation diagnostics would be lost, which is what an earlier
    // 37-proposal run did. Cancelling through the control lets the driver
    // return normally with everything it observed.
    let expired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let deadline = {
        let control = control.clone();
        let expired = Arc::clone(&expired);
        tokio::spawn(async move {
            tokio::time::sleep(budget).await;
            expired.store(true, std::sync::atomic::Ordering::Relaxed);
            control.cancel();
        })
    };

    let reported = driver
        .run_with_report(&host, &control, &reporter, Some(options))
        .await;
    deadline.abort();
    events.record(PhaseEvent::phase("tracking::finished"));

    if expired.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!(
            "bench: background share tracking hit its {budget:?} budget; the confirmation \
             tail below is incomplete. Raise --tracking-budget, or use \
             --confirm-concurrency to measure the tail's floor."
        );
    }

    let (report, snapshot) = reported.into_parts();
    let quiescence = if expired.load(std::sync::atomic::Ordering::Relaxed) {
        format!("BenchBudgetExpired({:?})", report.quiescence)
    } else {
        format!("{:?}", report.quiescence)
    };
    Ok((
        TrackingSummary {
            quiescence,
            passes: report.passes,
            confirmed: report.confirmed.len(),
            resubmitted: report.resubmitted.len(),
            ambiguous: report.ambiguous.len(),
            unrecoverable: report.unrecoverable.len(),
        },
        snapshot,
    ))
}

/// Shares the round has durably placed, for the delivery announcement.
///
/// Read from durable state rather than counted from the report: the report's
/// deliveries describe what this run dispatched, and a resumed round's earlier
/// placements are just as real.
fn placed_shares(database: &Arc<VotingDb>, round_id: &str) -> usize {
    zcash_voting::share::list(database, round_id)
        .map(|shares| shares.len())
        .unwrap_or_default()
}

/// Runs the concurrent focused-confirmation experiment over this round.
///
/// See [`crate::confirm`] for why this is a separate mode and what its numbers
/// are not. The helper client is built on the same shared route as delivery, so
/// a synthetic fleet applies here too.
async fn confirm_shares_concurrently(
    database: &Arc<VotingDb>,
    config: &BenchRunConfig,
    route: &Arc<BenchRoute>,
    events: &EventLog,
    budget: Duration,
) -> Result<crate::confirm::ConfirmationRun> {
    let client = HelperClient::new(
        Arc::new(HyperTransport::with_shared_route(Arc::clone(route))),
        HelperHealth::default(),
    );
    crate::confirm::confirm_concurrently(
        &crate::confirm::ConfirmationTarget {
            database,
            client: &client,
            round_id: &config.round_id,
            helper_urls: &config.endpoints.helper_urls,
        },
        config.confirm_concurrency,
        budget,
        events,
    )
    .await
}

/// Persists a sweep's per-share snapshots as one array.
///
/// One file rather than a thousand: each focused confirmation freezes its own
/// report, and their record ids are invocation-local, so they cannot be merged
/// into a single snapshot without colliding.
fn save_snapshots(run_dir: &std::path::Path, name: &str, snapshots: &[OperationObservability]) {
    match serde_json::to_vec(snapshots) {
        Ok(encoded) => {
            if let Err(error) = std::fs::write(run_dir.join(name), encoded) {
                eprintln!("bench: could not write {name}: {error}");
            }
        }
        Err(error) => eprintln!("bench: could not encode {name}: {error}"),
    }
}

/// Carries a previous round's cached PIR proofs and padded-slot secrets in.
///
/// Best effort: a cold run still works, it is only slower and far more exposed
/// to the single synchronous staging PIR endpoint. The proofs are keyed by
/// nullifier and are not round-specific, and the padded secrets are what make
/// the synthetic nullifiers stable across rounds so those proofs can hit.
fn seed_precompute(config: &BenchRunConfig, events: &EventLog) {
    let Some(template) = &config.warm_pir_from else {
        return;
    };
    match recovery_conformance::precompute::seed_precompute(
        &config.sidecar,
        template,
        &config.round_id,
        recovery_conformance::precompute::ProofCacheSeed::Warm,
    ) {
        Ok(seeded) => {
            eprintln!(
                "bench: seeded {} PIR proofs and {} padded-slot secret sets",
                seeded.proofs, seeded.padded_bundles
            );
            let mut phase = PhaseEvent::phase("setup::pir_seeded");
            phase.detail = Some(format!(
                "{} proofs, {} padded bundles",
                seeded.proofs, seeded.padded_bundles
            ));
            events.record(phase);
        }
        Err(error) => eprintln!("bench: precompute not seeded: {error}"),
    }
}

/// Persists one invocation snapshot beside the run it describes.
///
/// Best effort by design: a benchmark that refused to report its outcome
/// because it could not write a diagnostic would lose the run as well as the
/// diagnostic.
fn save_snapshot(run_dir: &std::path::Path, name: &str, snapshot: Option<OperationObservability>) {
    let Some(snapshot) = snapshot else {
        eprintln!("bench: no observability snapshot for {name}");
        return;
    };
    if snapshot.records_dropped > 0
        || snapshot.summary_updates_dropped > 0
        || snapshot.active_stages_dropped > 0
    {
        eprintln!(
            "bench: {name} dropped {} records, {} summary updates, {} stage starts; \
             raise --max-records for a complete capture",
            snapshot.records_dropped,
            snapshot.summary_updates_dropped,
            snapshot.active_stages_dropped
        );
    }
    match serde_json::to_vec(&snapshot) {
        Ok(encoded) => {
            if let Err(error) = std::fs::write(run_dir.join(name), encoded) {
                eprintln!("bench: could not write {name}: {error}");
            }
        }
        Err(error) => eprintln!("bench: could not encode {name}: {error}"),
    }
}

/// Flattens one driver event into the phase log.
///
/// Only the identity of the work is recorded — step kind, bundle, proposal,
/// share. A refreshed plan is reduced to its tally: the plan itself is large,
/// it is re-emitted on every pass, and none of it is a timing boundary.
fn round_event(event: &RoundDriveEvent) -> PhaseEvent {
    use zcash_voting::session::NextStep;

    let describe = |name: &str, step: &NextStep| {
        let mut phase = PhaseEvent::phase(name);
        phase.step = Some(format!("{:?}", step.kind_view()));
        match step {
            NextStep::Delegate { bundle_index }
            | NextStep::AdvanceDelegation { bundle_index }
            | NextStep::AdvanceImportedDelegation { bundle_index } => {
                phase.bundle_index = Some(*bundle_index);
            }
            NextStep::CastVote {
                bundle_index,
                proposal_id,
                ..
            }
            | NextStep::AdvanceVote {
                bundle_index,
                proposal_id,
            }
            | NextStep::AdvanceVoteBatch {
                bundle_index,
                proposal_id,
            } => {
                phase.bundle_index = Some(*bundle_index);
                phase.proposal_id = Some(*proposal_id);
            }
            NextStep::SubmitShares {
                bundle_index,
                proposal_id,
                share_index,
            }
            | NextStep::ConfirmShare {
                bundle_index,
                proposal_id,
                share_index,
            } => {
                phase.bundle_index = Some(*bundle_index);
                phase.proposal_id = Some(*proposal_id);
                phase.share_index = Some(*share_index);
            }
            _ => {}
        }
        phase
    };

    match event {
        RoundDriveEvent::PlanRefreshed { tally, .. } => {
            let mut phase = PhaseEvent::phase("round::plan_refreshed");
            phase.detail = Some(format!(
                "{}/{} proposals, {} obligations",
                tally.completed_proposals, tally.total_proposals, tally.remaining_obligations
            ));
            phase
        }
        RoundDriveEvent::StepSelected { step } => describe("round::step_selected", step),
        RoundDriveEvent::StepProgress { step, progress } => {
            let mut phase = describe("round::step_progress", step);
            phase.detail = Some(progress_label(progress).to_string());
            phase
        }
        RoundDriveEvent::StepFinished { step, disposition } => {
            let mut phase = describe("round::step_finished", step);
            phase.detail = Some(format!("{disposition:?}"));
            phase
        }
        RoundDriveEvent::StepFailed { step, kind, .. } => {
            let mut phase = describe("round::step_failed", step);
            phase.detail = Some(format!("{kind:?}"));
            phase
        }
        RoundDriveEvent::AwaitingRepoll { step, delay } => {
            let mut phase = describe("round::awaiting_repoll", step);
            phase.detail = Some(format!("{}ms", delay.as_millis()));
            phase
        }
        RoundDriveEvent::BundleSkipped { bundle_index, .. } => {
            let mut phase = PhaseEvent::phase("round::bundle_skipped");
            phase.bundle_index = Some(*bundle_index);
            phase
        }
        _ => PhaseEvent::phase("round::unknown_event"),
    }
}

/// Names a step's progress without depending on its `Debug` shape.
///
/// The payloads are large and several carry whole domain values; the log wants
/// the boundary, not the value that crossed it.
fn progress_label(progress: &zcash_voting::RoundStepProgress) -> &'static str {
    use zcash_voting::RoundStepProgress as P;
    match progress {
        P::Selected(_) => "selected",
        P::Delegation { .. } => "delegation",
        P::TreeSynced { .. } => "tree_synced",
        P::VoteCommit(_) => "vote_commit",
        P::DelegateAndVoteBatchPersisted { .. } => "batch_persisted",
        P::HelperPlansPrepared { .. } => "helper_plans_prepared",
        P::ChainOutcome(_) => "chain_outcome",
        P::ShareOutcome(_) => "share_outcome",
        P::ShareConfirmed { .. } => "share_confirmed",
        _ => "unknown",
    }
}

fn tracking_event(event: &ShareTrackingEvent) -> PhaseEvent {
    match event {
        ShareTrackingEvent::PassStarted { pass } => {
            let mut phase = PhaseEvent::phase("tracking::pass_started");
            phase.detail = Some(pass.to_string());
            phase
        }
        ShareTrackingEvent::PassFinished { pass, report } => {
            let mut phase = PhaseEvent::phase("tracking::pass_finished");
            phase.detail = Some(format!(
                "pass {pass}: {} confirmed, {} resubmitted, {} ambiguous",
                report.confirmed.len(),
                report.resubmitted.len(),
                report.ambiguous.len()
            ));
            phase
        }
        ShareTrackingEvent::PassFailed { pass, .. } => {
            let mut phase = PhaseEvent::phase("tracking::pass_failed");
            phase.detail = Some(pass.to_string());
            phase
        }
        ShareTrackingEvent::AwaitingNextPass { delay, .. } => {
            let mut phase = PhaseEvent::phase("tracking::awaiting_next_pass");
            phase.detail = Some(format!("{}ms", delay.as_millis()));
            phase
        }
        _ => PhaseEvent::phase("tracking::unknown_event"),
    }
}

/// Names the quiescence variant without depending on its `Debug` shape.
fn quiescence_kind(quiescence: &zcash_voting::round_drive::RoundQuiescence) -> String {
    use zcash_voting::round_drive::RoundQuiescence as Q;
    match quiescence {
        Q::NoWorkLeft => "NoWorkLeft",
        Q::NeedsBundleSetup => "NeedsBundleSetup",
        Q::PersistedChainTerminal => "PersistedChainTerminal",
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

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

fn voting_error(error: zcash_voting::VotingError) -> anyhow::Error {
    anyhow::anyhow!("{error:?}")
}
