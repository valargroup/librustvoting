//! Everything resolved before a round is provisioned.
//!
//! A benchmark run costs a real round on the staging chain — a delegation is
//! consumed per round and cannot be replayed — and forty minutes of proving.
//! A run that is going to fail for want of a credential, a wallet, or a
//! reachable config should fail in its first second, not after spending both.
//!
//! Every check here is the conformance suite's, for the same reasons. What is
//! not here is anything that mutates: this resolves, it never provisions.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use recovery_conformance::environment::Environment;
use recovery_conformance::provisioning::VoteManagerKeyring;
use recovery_conformance::stage_config::StageDeployment;

/// PIR endpoint the published snapshot is read from.
pub const PIR_BASE: &str = "https://stage.pir.valargroup.org";

/// Everything a run needs, resolved once.
pub struct Preflight {
    pub worker: PathBuf,
    pub wallet_db: PathBuf,
    pub warm_pir: Option<PathBuf>,
    pub account_uuid: String,
    pub keyring: VoteManagerKeyring,
    pub deployment: StageDeployment,
}

/// Resolves credentials, the wallet, the coordinator key, and the deployment.
///
/// Failing rather than skipping, unlike the conformance fixture: this is a
/// command a person ran deliberately, and a benchmark that quietly did nothing
/// would be indistinguishable from one that found nothing to report.
pub async fn resolve() -> Result<Preflight> {
    let worker = worker_binary().context(
        "the stage-bench worker is not built beside this binary; run `make stage-bench-worker`",
    )?;

    let deployment = fetch_deployment()
        .await
        .context("the published staging configuration is unreachable")?;
    let environment = Environment::from_env(deployment.clone())
        .map_err(|error| anyhow::anyhow!("credentials unavailable: {error}"))?;
    // The staging guard runs before anything is provisioned, not before each
    // broadcast: every convenience path that derives a chain id from a network
    // resolves to production, and this benchmark must never reach it.
    recovery_conformance::environment::assert_targets_staging(environment.chain_id());

    let wallet_db = wallet_path();
    if !wallet_db.exists() {
        bail!(
            "no scanned voter wallet at {}; build one with \
             `cargo run -p recovery-conformance --example sync_voter`",
            wallet_db.display()
        );
    }
    // Identity, not note count: two wallets on the same faucet can hold
    // identical amounts, and one such pair exists on the development host.
    let seed = recovery_conformance::signing::voter_seed()?;
    if !recovery_conformance::signing::wallet_matches_seed(&wallet_db, &seed)? {
        bail!(
            "the wallet at {} does not belong to the configured seed",
            wallet_db.display()
        );
    }

    let keyring = VoteManagerKeyring::import(environment.vote_manager_mnemonic())
        .context("the coordinator key is unusable")?;
    if keyring.address() != recovery_conformance::provisioning::COORDINATOR_ADDRESS {
        bail!(
            "the coordinator key derives {} rather than the registered coordinator",
            keyring.address()
        );
    }

    Ok(Preflight {
        worker,
        wallet_db,
        warm_pir: warm_pir_path(),
        account_uuid: account_uuid(),
        keyring,
        deployment,
    })
}

/// The scanned voter wallet.
///
/// Defaults to the conformance suite's cache deliberately: the same fixed voter
/// seed funds both, scanning it takes hours, and a second copy would double
/// that cost for no benefit.
pub fn wallet_path() -> PathBuf {
    std::env::var("STAGE_BENCH_WALLET")
        .or_else(|_| std::env::var("RECOVERY_CONFORMANCE_WALLET"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".cache/recovery-conformance/voter.db"))
}

/// The wallet's account UUID, which is also the sidecar's wallet scope.
pub fn account_uuid() -> String {
    std::env::var("STAGE_BENCH_ACCOUNT")
        .or_else(|_| std::env::var("RECOVERY_CONFORMANCE_ACCOUNT"))
        .unwrap_or_else(|_| "8b29d4e6-7940-4570-b2c2-3c7a25ba6922".to_string())
}

/// A previous run's cached PIR proofs, when one exists.
///
/// Shared with the conformance suite for the same reason the wallet is: proofs
/// are keyed by nullifier and are not round-specific, so a proof either suite
/// fetched is valid for the other.
pub fn warm_pir_path() -> Option<PathBuf> {
    let path = std::env::var("STAGE_BENCH_WARM_PIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".cache/recovery-conformance/pir-warm.db"));
    path.exists().then_some(path)
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
}

/// Locates the worker beside this binary.
fn worker_binary() -> Option<PathBuf> {
    let mut directory = std::env::current_exe().ok()?;
    directory.pop();
    for candidate in [directory.clone(), directory.parent()?.to_path_buf()] {
        let worker = candidate.join("stage-bench-worker");
        if worker.exists() {
            return Some(worker);
        }
    }
    None
}

/// Reads the published staging deployment.
///
/// Through `curl` rather than a client of our own, matching the conformance
/// suite: this is setup, and adding an HTTP stack to fetch one document would
/// be a second transport to keep working.
pub async fn fetch_deployment() -> Result<StageDeployment> {
    let output = std::process::Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "30",
            recovery_conformance::stage_config::STAGE_DYNAMIC_CONFIG_URL,
        ])
        .output()?;
    anyhow::ensure!(output.status.success(), "fetching the stage config failed");
    StageDeployment::from_json(&output.stdout)
        .map_err(|error| anyhow::anyhow!("parsing the stage config: {error}"))
}

/// Folds a finished run's PIR proofs back into the warm template.
///
/// Seeding is one-way without this, and an incomplete template stays
/// incomplete: the proofs it lacks are fetched live on every run, from the one
/// synchronous endpoint most likely to time out — which is also the fetch that
/// would have supplied them.
///
/// Safe to accumulate across rounds: proofs are keyed by nullifier and the
/// padded-slot secrets that generate the synthetic ones are copied *from* this
/// template into each new round, so `INSERT OR IGNORE` can never introduce a
/// stale proof. Best effort — a failure here costs speed on the next run and
/// nothing else.
pub fn refresh_warm_pir(template: &std::path::Path, sidecar: &std::path::Path) -> Result<usize> {
    let connection =
        rusqlite::Connection::open(template).context("opening the warm PIR template")?;
    connection
        .execute(
            "ATTACH DATABASE ?1 AS finished",
            rusqlite::params![sidecar.to_str().context("the sidecar path is not UTF-8")?],
        )
        .context("attaching the finished round")?;
    connection
        .execute(
            "INSERT OR IGNORE INTO pir_proof_cache SELECT * FROM finished.pir_proof_cache",
            [],
        )
        .context("copying cached PIR proofs")
}
