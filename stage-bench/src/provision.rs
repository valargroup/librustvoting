//! Getting a round on the staging chain to benchmark against.
//!
//! A round is one-shot: a delegation is consumed per round and cannot be
//! replayed, so every run needs its own. Creation also serialises chain-wide —
//! the chain refuses a second `CreateVotingSession` while one ceremony is
//! pending — so two benchmark runs must never provision concurrently.

use anyhow::{Context, Result};
use recovery_conformance::environment::{
    LIGHTWALLETD_URLS, STAGING_CHAIN_ID, STAGING_CHAIN_RPC, ZCASH_NETWORK,
};
use recovery_conformance::provisioning::{provision_round_with_ballot, ChainTarget};

use crate::ballot::Ballot;
use crate::preflight::{Preflight, PIR_BASE};

/// Attempts allowed when provisioning a round.
///
/// Provisioning is setup, not the thing under test: it posts a session to the
/// chain, waits for the ceremony, and resolves an anchor from lightwalletd.
/// Every one of those is a network call, and a transient failure in any of them
/// leaves the benchmark with no round — which is a wasted invocation rather
/// than a measurement.
const PROVISION_ATTEMPTS: usize = 3;

/// How long to wait before provisioning again.
///
/// A fresh round each time, which is safe and cheap: rounds are one-shot by
/// design, so an abandoned one costs nothing but its own id.
const PROVISION_RETRY_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// A freshly provisioned round and the vote end it was created with.
///
/// The vote end travels with the round because share scheduling derives its
/// overdue and last-moment windows from the distance to it. Recomputing it
/// later would silently disagree with what the chain was told.
pub struct ProvisionedRound {
    pub round_id: String,
    pub vote_end_time_seconds: u64,
}

/// Provisions a round over `ballot`, retrying transient staging failures.
pub async fn provision(
    preflight: &Preflight,
    ballot: &Ballot,
    vote_window_seconds: u64,
) -> Result<ProvisionedRound> {
    let mut last = None;
    for attempt in 1..=PROVISION_ATTEMPTS {
        match provision_once(preflight, ballot, vote_window_seconds).await {
            Ok(round) => return Ok(round),
            Err(error) => {
                eprintln!(
                    "bench: provisioning attempt {attempt}/{PROVISION_ATTEMPTS} failed: {error:#}"
                );
                last = Some(error);
                if attempt < PROVISION_ATTEMPTS {
                    tokio::time::sleep(PROVISION_RETRY_WAIT).await;
                }
            }
        }
    }
    Err(last
        .unwrap_or_else(|| anyhow::anyhow!("provisioning made no attempt"))
        .context(format!(
            "staging would not provision a round in {PROVISION_ATTEMPTS} attempts"
        )))
}

async fn provision_once(
    preflight: &Preflight,
    ballot: &Ballot,
    vote_window_seconds: u64,
) -> Result<ProvisionedRound> {
    let target = ChainTarget {
        rpc_url: STAGING_CHAIN_RPC,
        chain_id: STAGING_CHAIN_ID,
    };
    let vote_end = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
    )
    .context("the clock is outside the representable range")?
        + i64::try_from(vote_window_seconds).context("the vote window is too long")?;

    let round_id = provision_round_with_ballot(
        &preflight.keyring,
        PIR_BASE,
        LIGHTWALLETD_URLS[0],
        ZCASH_NETWORK,
        &target,
        vote_end,
        ballot.proposals().to_vec(),
    )
    .await?;

    Ok(ProvisionedRound {
        round_id,
        vote_end_time_seconds: vote_end as u64,
    })
}
