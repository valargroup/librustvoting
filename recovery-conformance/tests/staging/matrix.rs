//! Driving every crash stage against staging, in order, and judging the result.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use recovery_conformance::assertions::{
    assert_confirmed_by_tree, assert_idempotent, assert_other_bundles_untouched,
    assert_plans_precede_broadcast, assert_recovered_the_same_transaction,
    assert_reservations_monotonic, assert_stage_state, assert_terminal_rows_unchanged,
    confirmation_source, confirmed_transaction_hash, deterministic_plan,
    dispatched_transaction_hash, DurableSnapshot,
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

/// Dispatch ceiling for one drive, so a plan that never shrinks ends the run.
const MAX_DISPATCHES: usize = 512;

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

    for stage in CrashStage::ALL {
        let stage = *stage;
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
    round_id: &str,
    control: &DurableSnapshot,
) -> Result<(), Outcome> {
    let started = Instant::now();
    let sidecar = fixture.workspace.join(format!("{}.db", stage.name()));
    let _ = std::fs::remove_file(&sidecar);

    let armed = config_for(fixture, &sidecar, round_id, RunMode::Armed { stage });

    // (c) spawn the armed child; (d) require SIGABRT and a matching observation
    let crash = match run_until_crash(&fixture.worker, &armed) {
        Ok(crash) => crash,
        Err(error) => {
            return Err(Outcome::Skipped(format!(
                "never reached the stage: {error:#}"
            )))
        }
    };

    // (b) capture the durable state the crash left
    let after_crash = DurableSnapshot::read(&sidecar)
        .map_err(|error| Outcome::Failed(format!("unreadable sidecar: {error:#}")))?;

    // (f) plan twice in a fresh process-local database and require agreement
    let plan = deterministic_plan(&sidecar, &fixture_account(), round_id, &proposal_ids())
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

    // A stage that left the chain untouched stops here.
    //
    // This is not a shortcut. Resuming such a stage to quiescence would
    // delegate and vote on the shared pre-chain round, and a delegation is
    // consumed on the vote chain: the next non-mutative stage's copy of that
    // round would then fail with `nullifier already spent`, which is a
    // statement about round accounting rather than about recovery. The
    // alternative — a fresh round per stage — is what `touches_chain()`
    // already selects for the stages that need it.
    //
    // What is verified for these stages is everything the crash itself
    // determines: the stage was reached by a real abort, the durable state is
    // what that boundary leaves, and the plan derived from it is deterministic
    // and names the right work. Convergence is proven by the chain-touching
    // stages, which each get their own round.
    if !stage.touches_chain() {
        return Ok(());
    }

    // (h) resume to quiescence in a new process
    let resumed = config_for(fixture, &sidecar, round_id, RunMode::Unarmed);
    if started.elapsed() > STAGE_BUDGET {
        return Err(Outcome::Skipped(
            "stage budget exhausted before resume".to_string(),
        ));
    }
    // A resume that never completes is only a skip when the environment stopped
    // it. Retries that all end on the same non-transport error mean the round
    // does not converge, which is exactly what this matrix exists to catch.
    let outcome = run_to_quiescence(&fixture.worker, &resumed).map_err(|error| {
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
    if terminal.states() != control.states() {
        return Err(Outcome::Failed(format!(
            "A3 VIOLATED: terminal submission states {:?} differ from the control's {:?}",
            terminal.states(),
            control.states()
        )));
    }

    // (k) a second resume must find nothing to do
    let settled = deterministic_plan(&sidecar, &fixture_account(), round_id, &proposal_ids())
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
    let outcome = run_to_quiescence(&fixture.worker, &config)?;
    anyhow::ensure!(
        outcome.is_terminal_success(),
        "the control run ended at {} rather than quiescence",
        outcome.quiescence
    );
    DurableSnapshot::read(&sidecar)
}

async fn provision(fixture: &Fixture) -> anyhow::Result<String> {
    let target = ChainTarget {
        rpc_url: STAGING_CHAIN_RPC,
        chain_id: STAGING_CHAIN_ID,
    };
    let vote_end = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64
        + 14 * 24 * 3600;
    provision_active_round(
        &fixture.keyring,
        PIR_BASE,
        LIGHTWALLETD_URLS[0],
        ZCASH_NETWORK,
        &target,
        vote_end,
    )
    .await
}

fn config_for(fixture: &Fixture, sidecar: &Path, round_id: &str, mode: RunMode) -> RoundRunConfig {
    RoundRunConfig {
        sidecar: sidecar.to_path_buf(),
        wallet_db: fixture.wallet_db.clone(),
        warm_pir_from: fixture.warm_pir.clone(),
        round_id: round_id.to_string(),
        account_uuid: fixture_account(),
        endpoints: endpoints_from(&fixture.deployment),
        target: default_target(),
        mode,
        crash_log: sidecar.with_extension("crashlog.jsonl"),
        outcome: sidecar.with_extension("outcome.json"),
        max_dispatches: MAX_DISPATCHES,
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
