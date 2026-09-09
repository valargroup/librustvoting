//! The child that drives one benchmark round.
//!
//! Takes a single argument: the path to the run configuration the parent wrote.
//! Nothing secret is in that file — the credentials arrive through the inherited
//! environment, so they are never on disk and never in a `ps` listing.

use std::path::PathBuf;

use anyhow::{Context, Result};
use stage_bench::run_config::BenchRunConfig;

#[tokio::main]
async fn main() -> Result<()> {
    let path: PathBuf = std::env::args_os()
        .nth(1)
        .context("usage: stage-bench-worker <run-config.json>")?
        .into();
    let config = BenchRunConfig::read(&path).context("reading the run configuration")?;

    // Re-checked in the child, not only in the parent. The parent's guard
    // cannot cover a configuration file edited or reused between the two, and
    // the cost of being wrong is a benchmark broadcasting to the production
    // vote chain. The chain id the executor binds is a constant in `drive`;
    // what a stale configuration can still redirect is the endpoints, so those
    // are what this checks.
    anyhow::ensure!(
        config.endpoints.chain_rpc == recovery_conformance::environment::STAGING_CHAIN_RPC,
        "refusing to run: the configuration names {} rather than the staging chain RPC",
        config.endpoints.chain_rpc
    );

    let outcome = stage_bench::drive::drive(&config).await?;
    eprintln!(
        "bench: finished at {} with {} failures",
        outcome.quiescence_kind,
        outcome.failures.len()
    );
    Ok(())
}
