//! Driving every crash stage against staging, in order, and judging the result.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use recovery_conformance::assertions::{
    assert_confirmed_by_tree, assert_idempotent, assert_matches_control,
    assert_no_second_generation, assert_other_bundles_untouched, assert_plans_precede_broadcast,
    assert_recovered_the_same_transaction, assert_reservations_monotonic, assert_stage_state,
    assert_terminal_rows_unchanged, assert_untouched_bundles_did_not_reserve, confirmation_source,
    confirmed_transaction_hash, deterministic_plan, dispatched_transaction_hash, DurableSnapshot,
};
use recovery_conformance::child::{run_to_quiescence, run_until_crash};
use recovery_conformance::environment::{
    Environment, LIGHTWALLETD_URLS, STAGING_CHAIN_ID, STAGING_CHAIN_RPC, ZCASH_NETWORK,
};
use recovery_conformance::provisioning::{provision_active_round, ChainTarget, VoteManagerKeyring};
use recovery_conformance::round_run::{default_target, endpoints_from, proposal_ids};
use recovery_conformance::run_config::{RoundRunConfig, RunMode};
use recovery_conformance::stage_config::StageDeployment;
use recovery_conformance::CrashStage;

/// PIR endpoint the suite reads snapshots from.
const PIR_BASE: &str = "https://stage.pir.valargroup.org";

/// How long one stage may take before it is abandoned.
///
/// Generous: a full drive to quiescence proves three delegations and nine
/// votes, and a vote proof takes minutes. The budget exists so a wedged run
/// fails the matrix rather than hanging it.
const STAGE_BUDGET: Duration = Duration::from_secs(45 * 60);

/// How long after provisioning the round's vote closes.
///
/// Share recovery treats a share as overdue after a quarter of the remaining
/// vote window, so this also sets how long an interrupted attempt waits before
/// it may be retried — bounded by the timing policy the suite passes to
/// background tracking.
const VOTE_WINDOW_SECONDS: i64 = 14 * 24 * 3600;

/// Dispatch ceiling for one drive, so a plan that never shrinks ends the run.
///
/// Sized from the work a resume can actually owe, because the ceiling is a
/// livelock detector and a ceiling below the honest maximum turns every slow
/// convergence into a false positive. The round carries 3 bundles x 3 proposals
/// x 16 shares = 144 shares. A stage crashed at the first share POST resumes
/// owing every one of them: one dispatch to deliver each, then one per
/// confirmation poll, and a helper quorum routinely needs several polls before
/// it answers. At 512 that stage exhausted the budget with all 144 shares still
/// unconfirmed while the round was in fact converging.
///
/// Ten dispatches per share leaves room for delivery plus a long confirmation
/// tail. A plan that genuinely never shrinks still ends the run, just later.
const MAX_DISPATCHES: usize = 144 * 10;

pub enum Run {
    Skipped(String),
    Completed(Report),
}

pub struct Report {
    pub attempted: usize,
    pub passed: Vec<CrashStage>,
    pub failed: Vec<(CrashStage, String)>,
    pub skipped: Vec<(CrashStage, String)>,
}

impl Report {
    pub fn print(&self) {
        eprintln!("\n=== staging conformance ===");
        for stage in &self.passed {
            eprintln!("  PASS  {stage}");
        }
        for (stage, why) in &self.skipped {
            eprintln!("  SKIP  {stage}: {why}");
        }
        for (stage, why) in &self.failed {
            eprintln!("  FAIL  {stage}: {why}");
        }
        eprintln!(
            "  {} passed, {} failed, {} skipped, of {} attempted",
            self.passed.len(),
            self.failed.len(),
            self.skipped.len(),
            self.attempted
        );
    }
}

/// Everything the matrix needs, resolved once.
struct Fixture {
    worker: PathBuf,
    wallet_db: PathBuf,
    warm_pir: Option<PathBuf>,
    keyring: VoteManagerKeyring,
    deployment: StageDeployment,
    workspace: PathBuf,
}

pub fn run() -> Run {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => return Run::Skipped(format!("no tokio runtime: {error}")),
    };
    match runtime.block_on(prepare()) {
        Err(reason) => Run::Skipped(reason),
        Ok(fixture) => Run::Completed(runtime.block_on(drive_matrix(fixture))),
    }
}

/// Resolves credentials, the wallet, and the published deployment.
///
/// Anything missing skips the matrix rather than failing it: this file is one
/// test among several in a package that must stay runnable without staging.
async fn prepare() -> Result<Fixture, String> {
    let worker = worker_binary().ok_or_else(|| "worker binary not built".to_string())?;

    let deployment = fetch_deployment()
        .await
        .map_err(|error| format!("stage config unavailable: {error}"))?;
    let environment = Environment::from_env(deployment.clone())
        .map_err(|error| format!("credentials unavailable: {error}"))?;

    let wallet_db = wallet_path();
    if !wallet_db.exists() {
        return Err(format!(
            "no scanned voter wallet at {}; build one with the sync_voter example",
            wallet_db.display()
        ));
    }
    // Identity, not note count: two wallets on the same faucet can hold
    // identical amounts, and one such pair exists on the development host.
    let seed = recovery_conformance::signing::voter_seed()
        .map_err(|error| format!("seed unavailable: {error}"))?;
    match recovery_conformance::signing::wallet_matches_seed(&wallet_db, &seed) {
        Ok(true) => {}
        Ok(false) => {
            return Err(format!(
                "the wallet at {} does not belong to the configured seed",
                wallet_db.display()
            ))
        }
        Err(error) => return Err(format!("cannot verify the wallet: {error}")),
    }

    let keyring = VoteManagerKeyring::import(environment.vote_manager_mnemonic())
        .map_err(|error| format!("coordinator key unusable: {error}"))?;
    if keyring.address() != recovery_conformance::provisioning::COORDINATOR_ADDRESS {
        return Err(format!(
            "the coordinator key derives {} rather than the registered coordinator",
            keyring.address()
        ));
    }

    let workspace =
        std::env::temp_dir().join(format!("recovery-conformance-{}", std::process::id()));
    std::fs::create_dir_all(&workspace).map_err(|error| error.to_string())?;

    Ok(Fixture {
        worker,
        wallet_db,
        warm_pir: warm_pir_path(),
        keyring,
        deployment,
        workspace,
    })
}

async fn drive_matrix(fixture: Fixture) -> Report {
    let mut report = Report {
        attempted: 0,
        passed: Vec::new(),
        failed: Vec::new(),
        skipped: Vec::new(),
    };

    // The control comes first: every terminal comparison is against it, so a
    // matrix without one proves only that crashes converge somewhere.
    let control = match build_control(&fixture).await {
        Ok(control) => control,
        Err(error) => {
            report.failed.push((
                CrashStage::BeforeDelegation,
                format!("control run failed: {error:#}"),
            ));
            report.attempted = 1;
            return report;
        }
    };
    eprintln!("control terminal snapshot: {:?}", control.states());

    let selected = selected_stages();
    for stage in CrashStage::ALL {
        let stage = *stage;
        if let Some(selected) = &selected {
            if !selected.contains(&stage) {
                continue;
            }
        }
        report.attempted += 1;
        let started = Instant::now();

        // Every exercise resumes to quiescence, so every exercise eventually
        // mutates the chain even when its crash itself occurs before the first
        // POST. A round is one-shot once any bundle delegates; sharing one
        // would make later cases observe effects from an earlier case.
        let round = match provision(&fixture).await {
            Ok(round) => round,
            Err(error) => {
                report.skipped.push((stage, format!("no round: {error:#}")));
                continue;
            }
        };

        match exercise(&fixture, stage, &round, &control).await {
            Ok(()) => {
                eprintln!("  PASS {stage} in {:.0}s", started.elapsed().as_secs_f64());
                report.passed.push(stage);
            }
            // Printed as they happen, not only in the final report. A matrix
            // run takes tens of minutes, and a verdict withheld until the end
            // is indistinguishable from a stage that is still running.
            Err(Outcome::Skipped(why)) => {
                eprintln!(
                    "  SKIP {stage} after {:.0}s: {why}",
                    started.elapsed().as_secs_f64()
                );
                report.skipped.push((stage, why));
            }
            Err(Outcome::Failed(why)) => {
                eprintln!(
                    "  FAIL {stage} after {:.0}s: {why}",
                    started.elapsed().as_secs_f64()
                );
                report.failed.push((stage, why));
            }
        }
    }
    report
}

enum Outcome {
    Skipped(String),
    Failed(String),
}

/// Runs one stage end to end.
async fn exercise(
    fixture: &Fixture,
    stage: CrashStage,
    round: &ProvisionedRound,
    control: &DurableSnapshot,
) -> Result<(), Outcome> {
    let started = Instant::now();
    let sidecar = fixture.workspace.join(format!("{}.db", stage.name()));
    let _ = std::fs::remove_file(&sidecar);

    let armed = config_for(fixture, &sidecar, round, RunMode::Armed { stage });

    // (c) spawn the armed child; (d) require SIGABRT and a matching observation
    let crash = run_until_crash(&fixture.worker, &armed);
    warm_from(fixture, &sidecar);
    let crash = match crash {
        Ok(crash) => crash,
        Err(error) => {
            let detail = format!("{error:#}");
            // A stage that stops firing is the way this suite rots: it becomes
            // a skip, skips do not fail the matrix, and the run stays green
            // having proven nothing about that boundary. Only the stages known
            // to be unreachable may skip; for any other, a trigger that never
            // fires is a failure.
            if detail.contains("never reached") && !is_known_unreachable(stage) {
                return Err(Outcome::Failed(format!(
                    "{stage} was never reached, and it is not a stage known to be \
                     unreachable, so its crash seam has stopped firing: {detail}"
                )));
            }
            return Err(Outcome::Skipped(detail));
        }
    };

    // (b) capture the durable state the crash left
    let after_crash = DurableSnapshot::read(&sidecar)
        .map_err(|error| Outcome::Failed(format!("unreadable sidecar: {error:#}")))?;

    // (f) plan twice in a fresh process-local database and require agreement
    let plan = deterministic_plan(
        &sidecar,
        &fixture_account(),
        &round.round_id,
        &proposal_ids(),
    )
    .map_err(|error| Outcome::Failed(format!("{error:#}")))?;

    // (g) the stage's own durable expectations
    let bundle = default_target().bundle_index;
    assert_stage_state(stage, &plan, &after_crash, bundle)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    assert_plans_precede_broadcast(&after_crash)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    if stage.touches_chain() && crash.dispatched_a_post() {
        // A dispatched POST means a transaction may exist. Its record is the
        // only evidence of that, so this sidecar is never discarded or retried
        // past, whatever happens next.
        eprintln!(
            "  {stage}: a POST reached the wire; sidecar preserved at {}",
            sidecar.display()
        );
    }
    assert_other_bundles_untouched(&plan, bundle, 3)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;

    // Every stage resumes, including the ones that never reached the chain.
    //
    // They used to stop here, and the reason was sound while it held: a
    // pre-chain stage resumed on a *shared* round would delegate and vote on
    // it, and a delegation is consumed on the vote chain, so the next stage's
    // copy of that round would fail with `nullifier already spent` — a
    // statement about round accounting rather than about recovery.
    //
    // That sharing is gone. Every stage now provisions its own round, so a
    // pre-chain resume consumes only its own delegation and cannot reach
    // another stage. Stopping early would leave the whole delegation-side
    // family proving that the crash was real and its durable state correct,
    // while never proving the round recovers — no A2 convergence and no A3
    // equality with the control, for six of twenty stages, including
    // `before-broadcast`, the conservative-by-design case this suite exists
    // for.
    // (h) resume to quiescence in a new process
    let resumed = config_for(fixture, &sidecar, round, RunMode::Unarmed);
    if started.elapsed() > STAGE_BUDGET {
        return Err(Outcome::Skipped(
            "stage budget exhausted before resume".to_string(),
        ));
    }
    // A resume that never completes is only a skip when the environment stopped
    // it. Retries that all end on the same non-transport error mean the round
    // does not converge, which is exactly what this matrix exists to catch.
    let outcome = run_to_quiescence(&fixture.worker, &resumed);
    warm_from(fixture, &sidecar);
    let outcome = outcome.map_err(|error| {
        let detail = format!("{error:#}");
        if detail.contains("Transport") || detail.contains("PIR") {
            Outcome::Skipped(format!("resume did not complete: {detail}"))
        } else {
            Outcome::Failed(format!("resume never converged: {detail}"))
        }
    })?;

    // (i) fail on anything that is not a clean ending
    if !outcome.is_terminal_success() {
        return Err(Outcome::Failed(format!(
            "resume ended at {} rather than quiescence; failures: {:?}",
            outcome.quiescence, outcome.failures
        )));
    }

    let terminal = DurableSnapshot::read(&sidecar)
        .map_err(|error| Outcome::Failed(format!("unreadable sidecar: {error:#}")))?;
    assert_reservations_monotonic(&after_crash, &terminal)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    // The half the count cannot prove: no target gained a second generation,
    // and no bundle the crash left alone reserved another POST.
    assert_no_second_generation(&after_crash, &terminal)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    assert_untouched_bundles_did_not_reserve(&after_crash, &terminal, bundle)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    assert_terminal_rows_unchanged(&after_crash, &terminal)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;

    // Requirement 8 wants direct evidence that no second transaction was
    // POSTed, not an inference from eventual confirmation. The durable half is
    // the reservation count: every committed POST increments it and a trigger
    // makes it monotonic, so the number of reservations is the number of times
    // the wallet committed to sending. Reported per stage because the correct
    // value differs by boundary — a crash before dispatch legitimately reserves
    // again on resume, one after it must not — and asserting a number before
    // observing it would encode a guess.
    eprintln!(
        "  {stage}: reservations {} -> {} (crash -> terminal), states {:?}",
        after_crash.total_reservations(),
        terminal.total_reservations(),
        terminal.states()
    );

    // The mechanism, for the one stage that can only have used it: a crash
    // after dispatch but before the response was read leaves no candidate hash,
    // so confirmation can only come from scanning the tree.
    if stage == CrashStage::AfterBroadcastUnread {
        let source =
            confirmation_source(&sidecar).map_err(|error| Outcome::Failed(format!("{error:#}")))?;
        eprintln!("  {stage}: confirmation source {:?}", source.as_deref());
        assert_confirmed_by_tree(source.as_deref())
            .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    }

    // Requirement 8's chain-identity half, where the stage captured one. The
    // reservation count above says how many times the wallet committed to
    // sending; this says the thing that actually confirmed is the thing it
    // sent, which counting alone cannot show.
    if let Some(body) = crash.dispatched_response_body() {
        if let Some(dispatched) = dispatched_transaction_hash(body) {
            let confirmed = confirmed_transaction_hash(&sidecar)
                .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
            let source = confirmation_source(&sidecar)
                .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
            eprintln!(
                "  {stage}: dispatched {dispatched}, confirmed {} via {}",
                confirmed.as_deref().unwrap_or("<none>"),
                source.as_deref().unwrap_or("<none>")
            );
            assert_recovered_the_same_transaction(
                &dispatched,
                confirmed.as_deref(),
                source.as_deref(),
            )
            .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
        }
    }

    // (j) the terminal shape must match the uncrashed control
    if let Err(error) = assert_matches_control(&terminal, control) {
        return Err(Outcome::Failed(format!("{error:#}")));
    }

    // (k) a second resume must find nothing to do
    let settled = deterministic_plan(
        &sidecar,
        &fixture_account(),
        &round.round_id,
        &proposal_ids(),
    )
    .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    assert_idempotent(&settled).map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    Ok(())
}

/// Drives a fresh round to quiescence with no crash armed.
async fn build_control(fixture: &Fixture) -> anyhow::Result<DurableSnapshot> {
    let round = provision(fixture).await?;
    let sidecar = fixture.workspace.join("control.db");
    let _ = std::fs::remove_file(&sidecar);
    let config = config_for(fixture, &sidecar, &round, RunMode::Unarmed);
    let outcome = run_to_quiescence(&fixture.worker, &config);
    // Before the outcome is judged: a run that failed still fetched whatever
    // PIR proofs it got through, and those are exactly what the next run needs
    // in order not to fail the same way.
    warm_from(fixture, &sidecar);
    let outcome = outcome?;
    anyhow::ensure!(
        outcome.is_terminal_success(),
        "the control run ended at {} rather than quiescence",
        outcome.quiescence
    );
    DurableSnapshot::read(&sidecar)
}

/// Folds whatever a finished sidecar learned back into the warm template.
///
/// Called for every worker run, successful or not, and deliberately so. The
/// template began life holding proofs for the two full bundles only, and the
/// third bundle's slots went to the PIR fleet on every run and timed out there
/// — which is also the fetch that would have supplied them. Refreshing only
/// after a clean run cannot break that cycle, because the cycle is what stops
/// a run from being clean. A failed attempt still caches the proofs it did
/// retrieve, so accumulating across attempts converges on a complete template.
fn warm_from(fixture: &Fixture, sidecar: &Path) {
    let Some(template) = &fixture.warm_pir else {
        return;
    };
    match refresh_warm_pir(template, sidecar) {
        Ok(added) if added > 0 => eprintln!("run: warmed {added} more PIR proofs"),
        Ok(_) => {}
        Err(error) => eprintln!("run: could not refresh the PIR template: {error}"),
    }
}

/// Folds a completed round's PIR proofs back into the warm template.
///
/// Seeding is one-way without this, and an incomplete template stays
/// incomplete: the proofs it lacks are fetched live every run, and the bundle
/// they belong to is the one that times out — which is also the fetch that
/// would have supplied them. The observed shape was a template holding proofs
/// for the two full bundles only, so the third bundle's slots went to the PIR
/// fleet on every single run and failed there repeatedly.
///
/// Safe to accumulate across rounds for the same reason seeding is: proofs are
/// keyed by nullifier, the padded-slot secrets that generate the synthetic ones
/// are copied *from* this template into each new round, and a real note's
/// nullifier does not change. `INSERT OR IGNORE` never rewrites an existing
/// row, so a stale proof cannot be introduced by refreshing.
///
/// Best-effort: a failure here costs speed on the next run, nothing else.
fn refresh_warm_pir(
    template: &std::path::Path,
    sidecar: &std::path::Path,
) -> anyhow::Result<usize> {
    let connection = rusqlite::Connection::open(template)
        .map_err(|error| anyhow::anyhow!("opening the template: {error}"))?;
    connection
        .execute(
            "ATTACH DATABASE ?1 AS finished",
            rusqlite::params![sidecar
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("sidecar path is not UTF-8"))?],
        )
        .map_err(|error| anyhow::anyhow!("attaching the finished round: {error}"))?;
    let added = connection
        .execute(
            "INSERT OR IGNORE INTO pir_proof_cache SELECT * FROM finished.pir_proof_cache",
            [],
        )
        .map_err(|error| anyhow::anyhow!("copying cached PIR proofs: {error}"))?;
    Ok(added)
}

/// Stages whose crash seam cannot fire, with the reason.
///
/// Empty, and it should stay that way. `AfterVoteCommit` was listed here on the
/// belief that vote completion offered no seam between persisting the committed
/// vote and writing helper plans. It does: the step probes the helper fleet
/// between those two commits, and that probe is a real network round trip this
/// suite already wraps. Believing a boundary untestable is cheaper than
/// checking, and it cost this stage every run it was ever skipped in.
///
/// Everything must crash where it claims to, or the matrix fails rather than
/// skipping.
fn is_known_unreachable(_stage: CrashStage) -> bool {
    false
}

async fn provision(fixture: &Fixture) -> anyhow::Result<ProvisionedRound> {
    let target = ChainTarget {
        rpc_url: STAGING_CHAIN_RPC,
        chain_id: STAGING_CHAIN_ID,
    };
    let vote_end = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64
        + VOTE_WINDOW_SECONDS;
    let round_id = provision_active_round(
        &fixture.keyring,
        PIR_BASE,
        LIGHTWALLETD_URLS[0],
        ZCASH_NETWORK,
        &target,
        vote_end,
    )
    .await?;
    Ok(ProvisionedRound {
        round_id,
        vote_end_time_seconds: vote_end as u64,
    })
}

/// A freshly provisioned round and the vote end it was created with.
///
/// The vote end travels with the round because share recovery derives its
/// retry window from the distance to it. Recomputing it later would silently
/// disagree with what the chain was told.
struct ProvisionedRound {
    round_id: String,
    vote_end_time_seconds: u64,
}

fn config_for(
    fixture: &Fixture,
    sidecar: &Path,
    round: &ProvisionedRound,
    mode: RunMode,
) -> RoundRunConfig {
    RoundRunConfig {
        sidecar: sidecar.to_path_buf(),
        wallet_db: fixture.wallet_db.clone(),
        warm_pir_from: fixture.warm_pir.clone(),
        round_id: round.round_id.clone(),
        account_uuid: fixture_account(),
        endpoints: endpoints_from(&fixture.deployment),
        target: default_target(),
        mode,
        crash_log: sidecar.with_extension("crashlog.jsonl"),
        outcome: sidecar.with_extension("outcome.json"),
        max_dispatches: MAX_DISPATCHES,
        vote_end_time_seconds: round.vote_end_time_seconds,
    }
}

/// The wallet's account UUID, which is also the sidecar's wallet scope.
fn fixture_account() -> String {
    std::env::var("RECOVERY_CONFORMANCE_ACCOUNT")
        .unwrap_or_else(|_| "8b29d4e6-7940-4570-b2c2-3c7a25ba6922".to_string())
}

fn wallet_path() -> PathBuf {
    std::env::var("RECOVERY_CONFORMANCE_WALLET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".cache/recovery-conformance/voter.db"))
}

/// The stages this run exercises, or `None` for the whole matrix.
///
/// Set `RECOVERY_CONFORMANCE_STAGES` to a comma-separated list of stage names
/// to re-run only the stages a change could have affected. The control run is
/// unconditional, because every terminal comparison is against it.
///
/// An unrecognized name is a hard error rather than an empty selection: a typo
/// that silently ran nothing would report a green matrix having tested nothing,
/// which is the failure mode this suite exists to avoid.
fn selected_stages() -> Option<Vec<CrashStage>> {
    let requested = std::env::var("RECOVERY_CONFORMANCE_STAGES").ok()?;
    let stages: Vec<CrashStage> = requested
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            name.parse::<CrashStage>().unwrap_or_else(|_| {
                panic!(
                    "RECOVERY_CONFORMANCE_STAGES names an unknown stage {name:?}; \
                     known stages are {}",
                    CrashStage::ALL
                        .iter()
                        .map(|stage| stage.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
        })
        .collect();
    (!stages.is_empty()).then_some(stages)
}

fn warm_pir_path() -> Option<PathBuf> {
    let path = home().join(".cache/recovery-conformance/pir-warm.db");
    path.exists().then_some(path)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
}

/// Locates the worker beside the test binary.
fn worker_binary() -> Option<PathBuf> {
    let mut directory = std::env::current_exe().ok()?;
    directory.pop();
    for candidate in [directory.clone(), directory.parent()?.to_path_buf()] {
        let worker = candidate.join("recovery-conformance-worker");
        if worker.exists() {
            return Some(worker);
        }
    }
    None
}

async fn fetch_deployment() -> anyhow::Result<StageDeployment> {
    let output = std::process::Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "30",
            recovery_conformance::stage_config::STAGE_DYNAMIC_CONFIG_URL,
        ])
        .output()?;
    anyhow::ensure!(output.status.success(), "fetching the stage config failed");
    Ok(StageDeployment::from_json(&output.stdout)?)
}
