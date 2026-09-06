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

use anyhow::{Context, Result};

use zcash_voting::{
    delegate::{gather_delegation_lwd_inputs, ResolveDelegationLwdParams},
    delegation_pipeline::{DelegationPipeline, SqliteWalletDbOpener},
    round::VotingDb,
    round_drive::{FailureIsolation, RoundDrivePolicy, RoundDriver, RoundHostSource},
    session::Decision,
    BallotIntent, ChainAdvancePolicy, ChainSubmissionClientConfig, ChainSubmissionControl,
    DelegationSigner, DelegationStepInputs, HelperClient, HelperHealth, HyperTransport, PirFleet,
    ProposalRosterEntry, RoundBinding, RoundExecutor, RoundHostContext,
};

use crate::child::{CrashHelperTransport, CrashLog, CrashReporter, CrashTarget, CrashTransport};
use crate::environment::ZCASH_NETWORK;
use crate::provisioning::fetch_round;
use crate::run_config::{Endpoints, FailureRecord, RoundRunConfig, RunOutcome, Target};
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
    let layout = database
        .ensure_bundles(&config.round_id, &selected.voting_note_infos())
        .map_err(voting_error)?;
    eprintln!(
        "run: {} notes -> {} bundles",
        selected.notes.len(),
        layout.bundle_count
    );

    // Seeded after the bundles exist and before the driver precomputes, which
    // is the only window where it can take effect.
    if let Some(template) = &config.warm_pir_from {
        match seed_precompute(&config.sidecar, template, &config.round_id) {
            Ok(seeded) => eprintln!(
                "run: seeded {} PIR proofs and {} padded-slot secret sets",
                seeded.proofs, seeded.padded_bundles
            ),
            // Not fatal: a cold run still works, it is only slower and more
            // exposed to a stalled endpoint.
            Err(error) => eprintln!("run: precompute not seeded: {error}"),
        }
    }

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

    let armed = config.armed_stage();
    // The crash seams wrap the same transports a host would use, so the
    // requests that reach staging are real. An unarmed run passes `None` and
    // the wrappers become pass-throughs, which keeps the control run on the
    // same code path rather than a parallel one.
    let route = Arc::new(zcash_voting::transport::DirectRoute::default());
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

    let executor = RoundExecutor::with_transport(
        Arc::clone(&database),
        chain_transport,
        ChainSubmissionClientConfig::for_network(
            ZCASH_NETWORK,
            config.endpoints.vote_servers.clone(),
        )
        .with_vote_chain_id(crate::environment::STAGING_CHAIN_ID),
        helper_client,
    )
    .map_err(|error| anyhow::anyhow!("building the executor: {error:?}"))?
    .with_binding(RoundBinding {
        round_id: config.round_id.clone(),
        network: ZCASH_NETWORK,
        proposals: roster(),
        hotkey_secret: Some(zeroize::Zeroizing::new(hotkey.stored_secret().to_vec())),
    })
    .map_err(voting_error)?;

    executor
        .set_ballot_intents(&ballot())
        .map_err(voting_error)?;

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
        delegation: DelegationStepInputs {
            driver: Arc::new(pipeline),
            signer: DelegationSigner::Software(signing::software_signer(seed)),
            pir,
        },
        chain_policy: chain_policy_for(armed),
    };

    let reporter = CrashReporter::new(
        armed,
        CrashTarget {
            bundle_index: config.target.bundle_index,
            proposal_id: config.target.proposal_id,
        },
        Arc::clone(&log),
    );
    let control = ChainSubmissionControl::new(0);
    let report = RoundDriver::new(&executor)
        .with_policy(policy(armed.is_some(), config.max_dispatches))
        .run(&host, &control, &reporter)
        .await;

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
    };
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
    delegation: DelegationStepInputs,
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
/// and it is applied only for the stage that needs it, so every other run keeps
/// the shipped cadence.
fn chain_policy_for(armed: Option<CrashStage>) -> ChainAdvancePolicy {
    if armed == Some(CrashStage::AfterTracking) {
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
            delegation: Some(self.delegation.clone()),
            chain_policy: self.chain_policy.clone(),
            max_proof_concurrency: 1,
        }
    }
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

fn pir_layout() -> zcash_voting::config::PirLayout {
    zcash_voting::config::PirLayout {
        pir_depth: 19,
        tier0_layers: 12,
        tier1_layers: 7,
        poly_len: 4096,
    }
}

/// What a template supplied.
pub struct SeededPrecompute {
    pub proofs: usize,
    pub padded_bundles: usize,
}

/// Carries a previous round's precompute into this one.
///
/// Two things are copied, and the second is what makes the first useful:
///
/// - `pir_proof_cache` rows, keyed by `(wallet, network, tree root, nullifier)`
///   and therefore **not** round-specific: a proof fetched for one round is
///   valid for any round over the same snapshot.
/// - `bundles.padded_note_secrets`, which decide the dummy nullifiers a bundle
///   pads its slots with. `ensure_padded_secrets` samples them only when
///   absent, so seeding them makes this round pad with the same nullifiers the
///   cached proofs were fetched for. Without this the cache misses every padded
///   slot and the run refetches them — which is exactly what an earlier version
///   of this suite did, seeding proofs that could never be hit.
///
/// The privacy cost is real and bounded to this harness: padding exists so an
/// observer cannot tell a bundle's real notes from its dummies, and reusing
/// dummies across rounds lets one correlate them. That is acceptable only for a
/// disposable test wallet whose seed is shared with a test suite, and must
/// never be done for a wallet holding anything.
fn seed_precompute(
    sidecar: &std::path::Path,
    template: &std::path::Path,
    round_id: &str,
) -> Result<SeededPrecompute> {
    let connection = rusqlite::Connection::open(sidecar).context("opening the sidecar")?;
    connection
        .execute(
            "ATTACH DATABASE ?1 AS warm",
            rusqlite::params![template.to_str().context("template path is not UTF-8")?],
        )
        .context("attaching the template")?;

    let proofs = connection
        .execute(
            "INSERT OR IGNORE INTO pir_proof_cache SELECT * FROM warm.pir_proof_cache",
            [],
        )
        .context("copying cached PIR proofs")?;

    // Matched by bundle index within the wallet: bundle rows are round-keyed,
    // so the secrets move across rounds rather than being copied wholesale.
    let padded_bundles = connection
        .execute(
            "UPDATE bundles SET padded_note_secrets = (
                 SELECT w.padded_note_secrets FROM warm.bundles w
                 WHERE w.bundle_index = bundles.bundle_index
                   AND w.wallet_id = bundles.wallet_id
                   AND w.padded_note_secrets IS NOT NULL
             )
             WHERE round_id = ?1
               AND padded_note_secrets IS NULL
               AND EXISTS (
                 SELECT 1 FROM warm.bundles w
                 WHERE w.bundle_index = bundles.bundle_index
                   AND w.wallet_id = bundles.wallet_id
                   AND w.padded_note_secrets IS NOT NULL
               )",
            rusqlite::params![round_id],
        )
        .context("copying padded-slot secrets")?;

    let _ = connection.execute("DETACH DATABASE warm", []);
    Ok(SeededPrecompute {
        proofs,
        padded_bundles,
    })
}

fn voting_error(error: zcash_voting::VotingError) -> anyhow::Error {
    anyhow::anyhow!("{error:?}")
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
