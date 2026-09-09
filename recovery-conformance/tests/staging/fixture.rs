//! Everything three staging matrices need in common.
//!
//! Extracted rather than duplicated. A second copy of round provisioning or of
//! the wallet check would be a second definition of what the suite runs
//! against, and the two would drift in exactly the way that makes a green run
//! meaningless — the fault axes must exercise the *same* round shape, the same
//! wallet, and the same deployment the crash matrix does, or their comparison
//! against a control proves nothing.
//!
//! Included by `#[path]` into each matrix binary, which is how this crate
//! already shares `staging/matrix.rs`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use recovery_conformance::assertions::DurableSnapshot;
use recovery_conformance::child::run_to_quiescence;
use recovery_conformance::environment::{
    Environment, LIGHTWALLETD_URLS, STAGING_CHAIN_ID, STAGING_CHAIN_RPC, ZCASH_NETWORK,
};
use recovery_conformance::helper_fleet::HelperFleetPlan;
use recovery_conformance::provisioning::{provision_active_round, ChainTarget, VoteManagerKeyring};
use recovery_conformance::round_run::{default_target, endpoints_with_fleet};
use recovery_conformance::run_config::{RoundRunConfig, RunMode};
use recovery_conformance::stage_config::StageDeployment;
use recovery_conformance::stall::StallPlan;

/// PIR endpoint the suite reads snapshots from.
pub const PIR_BASE: &str = "https://stage.pir.valargroup.org";

/// How long after provisioning the round's vote closes.
///
/// One hour leaves room for a bounded stall and recovery while keeping test
/// rounds from remaining active on staging for days. Share retries still use
/// the host's explicit 45-second timing policy, independently of this window.
pub const VOTE_WINDOW_SECONDS: i64 = 60 * 60;

/// Everything the matrix needs, resolved once.
pub struct Fixture {
    pub worker: PathBuf,
    pub wallet_db: PathBuf,
    pub warm_pir: Option<PathBuf>,
    pub keyring: VoteManagerKeyring,
    pub deployment: StageDeployment,
    pub workspace: PathBuf,
}

/// Resolves credentials, the wallet, and the published deployment.
///
/// Anything missing skips the matrix rather than failing it: this file is one
/// test among several in a package that must stay runnable without staging.
pub async fn prepare() -> Result<Fixture, String> {
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

/// Folds whatever a finished sidecar learned back into the warm template.
///
/// Called for every worker run, successful or not, and deliberately so. The
/// template began life holding proofs for the two full bundles only, and the
/// third bundle's slots went to the PIR fleet on every run and timed out there
/// — which is also the fetch that would have supplied them. Refreshing only
/// after a clean run cannot break that cycle, because the cycle is what stops
/// a run from being clean. A failed attempt still caches the proofs it did
/// retrieve, so accumulating across attempts converges on a complete template.
pub fn warm_from(fixture: &Fixture, sidecar: &Path) {
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
pub fn refresh_warm_pir(
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

/// Attempts allowed when provisioning a round.
///
/// Provisioning is setup, not the thing under test: it posts a session to the
/// chain, waits for the ceremony, and resolves an anchor from lightwalletd.
/// Every one of those is a network call, and a transient failure in any of them
/// leaves the exercise with no round — which the matrices correctly refuse to
/// treat as a pass, and correctly refuse to skip past.
///
/// Without a retry, one momentary staging hiccup discards a three-hour run.
/// That happened: a pass ended with two stages skipped, one because a
/// provisioning transaction "never reported a round id" and one because a
/// lightwalletd channel would not open. Neither says anything about recovery.
const PROVISION_ATTEMPTS: usize = 3;

/// How long to wait before provisioning again.
///
/// A fresh round each time, which is safe and cheap: rounds are one-shot by
/// design, so an abandoned one costs nothing but its own id.
const PROVISION_RETRY_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

pub async fn provision(fixture: &Fixture) -> anyhow::Result<ProvisionedRound> {
    let mut last = None;
    for attempt in 1..=PROVISION_ATTEMPTS {
        match provision_once(fixture).await {
            Ok(round) => return Ok(round),
            Err(error) => {
                eprintln!(
                    "  provisioning attempt {attempt}/{PROVISION_ATTEMPTS} failed: {error:#}"
                );
                last = Some(error);
                if attempt < PROVISION_ATTEMPTS {
                    tokio::time::sleep(PROVISION_RETRY_WAIT).await;
                }
            }
        }
    }
    // Reported as a provisioning failure rather than a bare error, so a reader
    // of the matrix report can tell "staging would not give us a round" from
    // "the round did not recover".
    Err(last
        .unwrap_or_else(|| anyhow::anyhow!("provisioning made no attempt"))
        .context(format!(
            "staging would not provision a round in {PROVISION_ATTEMPTS} attempts"
        )))
}

async fn provision_once(fixture: &Fixture) -> anyhow::Result<ProvisionedRound> {
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
pub struct ProvisionedRound {
    pub round_id: String,
    pub vote_end_time_seconds: u64,
}

/// The wallet's account UUID, which is also the sidecar's wallet scope.
pub fn fixture_account() -> String {
    std::env::var("RECOVERY_CONFORMANCE_ACCOUNT")
        .unwrap_or_else(|_| "8b29d4e6-7940-4570-b2c2-3c7a25ba6922".to_string())
}

pub fn wallet_path() -> PathBuf {
    std::env::var("RECOVERY_CONFORMANCE_WALLET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".cache/recovery-conformance/voter.db"))
}

pub fn warm_pir_path() -> Option<PathBuf> {
    let path = home().join(".cache/recovery-conformance/pir-warm.db");
    path.exists().then_some(path)
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
}

/// Locates the worker beside the test binary.
pub fn worker_binary() -> Option<PathBuf> {
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

pub async fn fetch_deployment() -> anyhow::Result<StageDeployment> {
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

/// The faults one run is configured with.
///
/// Grouped rather than passed as two arguments so a call site reads as "this
/// run has these faults" and cannot transpose them, and so a matrix that grows
/// a third axis adds a field rather than another positional parameter every
/// existing caller has to be edited for.
#[derive(Clone, Debug, Default)]
pub struct Faults {
    pub stall: StallPlan,
    pub fleet: HelperFleetPlan,
}

impl Faults {
    /// A run with nothing injected: the real fleet, and no request stalled.
    pub fn none() -> Self {
        Self::default()
    }

    /// A run against `fleet` with no request stalled.
    pub fn fleet(fleet: HelperFleetPlan) -> Self {
        Self {
            stall: StallPlan::none(),
            fleet,
        }
    }

    /// A run with `stall` armed against the real fleet.
    pub fn stall(stall: StallPlan) -> Self {
        Self {
            stall,
            fleet: HelperFleetPlan::none(),
        }
    }
}

/// Builds the configuration for one child run.
///
/// `faults` is what distinguishes the three matrices; everything else is the
/// same round, wallet, and deployment, which is what makes their controls
/// comparable at all.
pub fn config_for(
    fixture: &Fixture,
    sidecar: &Path,
    round: &ProvisionedRound,
    mode: RunMode,
    max_dispatches: usize,
    faults: &Faults,
) -> RoundRunConfig {
    // A resumed run gets its own crash log. The armed run's log is read before
    // the resume starts, but the fleet assertions need the *resume's* record of
    // which helpers it contacted, and `CrashLog::create` truncates.
    let log_suffix = match mode {
        RunMode::Armed { .. } => "crashlog.jsonl",
        RunMode::Unarmed => "resume.crashlog.jsonl",
        RunMode::RecoverCombined => "signerless.crashlog.jsonl",
        RunMode::ObserveHelperOutage => "outage.crashlog.jsonl",
    };
    RoundRunConfig {
        sidecar: sidecar.to_path_buf(),
        wallet_db: fixture.wallet_db.clone(),
        warm_pir_from: fixture.warm_pir.clone(),
        round_id: round.round_id.clone(),
        account_uuid: fixture_account(),
        endpoints: endpoints_with_fleet(&fixture.deployment, &faults.fleet),
        target: default_target(),
        mode,
        crash_log: sidecar.with_extension(log_suffix),
        outcome: sidecar.with_extension("outcome.json"),
        max_dispatches,
        vote_end_time_seconds: round.vote_end_time_seconds,
        stall: faults.stall.clone(),
        fleet: faults.fleet.clone(),
    }
}

/// Drives a fresh round to quiescence with no fault armed.
///
/// The control every terminal comparison is made against. It takes `faults` so
/// a fleet matrix can build a control against *its own* fleet: comparing a
/// ten-helper round to a one-helper control would compare two different rounds
/// and call the difference a finding.
pub async fn build_control(
    fixture: &Fixture,
    max_dispatches: usize,
    faults: &Faults,
) -> anyhow::Result<DurableSnapshot> {
    let round = provision(fixture).await?;
    let sidecar = fixture.workspace.join("control.db");
    let _ = std::fs::remove_file(&sidecar);
    let config = config_for(
        fixture,
        &sidecar,
        &round,
        RunMode::Unarmed,
        max_dispatches,
        faults,
    );
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
    let snapshot = DurableSnapshot::read(&sidecar)?;
    recovery_conformance::combined::assert_combined_terminal(
        &snapshot,
        &recovery_conformance::round_run::proposal_ids(),
    )?;
    Ok(snapshot)
}
