//! Proposing and approving the round on the vote chain.
//!
//! Creating a round is not a single transaction. `MsgCreateVotingSession` is
//! gated behind coordinator approval, so the coordinator key *proposes* an
//! action and the chain holds it pending until enough coordinators approve.
//! Staging has six registered coordinators, so how many approvals a proposal
//! needs is a property of the deployment, not of this suite — a run that can
//! only supply its own approval will stall at the pending stage rather than
//! fail, and that distinction is what [`ProposalOutcome`] carries.
//!
//! Every call goes through the `svoted` binary rather than a hand-rolled cosmos
//! client. The message types are the chain's own, and encoding them here would
//! mean a second implementation that silently drifts the first time the proto
//! changes.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::keyring::VoteManagerKeyring;

/// Where a proposal ended up.
#[derive(Debug)]
pub enum ProposalOutcome {
    /// The chain accepted the proposal and it is awaiting approvals.
    Pending { transaction_hash: String },
    /// The proposal was accepted and no further approval is required.
    Applied { transaction_hash: String },
}

/// How a chain call is dispatched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispatch {
    /// Simulate only. Validates the message against live chain state without
    /// changing anything, which is how a round description should be checked
    /// before it consumes the one pending-ceremony slot.
    DryRun,
    /// Sign and broadcast.
    Broadcast,
}

/// Connection details for one chain call.
pub struct ChainTarget<'a> {
    pub rpc_url: &'a str,
    pub chain_id: &'a str,
}

/// Proposes `MsgCreateVotingSession` from the round description at `path`.
///
/// Refuses to target anything but the staging chain: this suite exists to kill
/// processes mid-broadcast, and the guard costs one string comparison.
pub fn propose_round(
    keyring: &VoteManagerKeyring,
    description: &Path,
    target: &ChainTarget<'_>,
    dispatch: Dispatch,
) -> Result<ProposalOutcome> {
    crate::environment::assert_targets_staging(target.chain_id);

    let mut args = vec![
        "tx".to_string(),
        "vote".to_string(),
        "create-voting-session".to_string(),
        description
            .to_str()
            .context("round description path is not UTF-8")?
            .to_string(),
        "--node".to_string(),
        target.rpc_url.to_string(),
        "--chain-id".to_string(),
        target.chain_id.to_string(),
        "--output".to_string(),
        "json".to_string(),
    ];
    match dispatch {
        // Simulation cannot open a keyring, so the signer is named by address
        // rather than by key name. Passing the key name here fails with a
        // bech32 decode error that reads like a malformed address.
        Dispatch::DryRun => {
            args.push("--from".to_string());
            args.push(keyring.address().to_string());
            args.push("--dry-run".to_string());
        }
        Dispatch::Broadcast => {
            args.extend(keyring.signing_flags());
            args.extend(gas_flags());
            args.push("--yes".to_string());
        }
    }

    let output = svoted(&args)?;
    if dispatch == Dispatch::DryRun {
        return Ok(ProposalOutcome::Pending {
            transaction_hash: String::new(),
        });
    }
    interpret_broadcast(&output)
}

/// Approves a pending coordinator action by id.
pub fn approve_action(
    keyring: &VoteManagerKeyring,
    action_id: &str,
    target: &ChainTarget<'_>,
) -> Result<ProposalOutcome> {
    crate::environment::assert_targets_staging(target.chain_id);

    let mut args = vec![
        "tx".to_string(),
        "vote".to_string(),
        "approve-coordinator-action".to_string(),
        action_id.to_string(),
        "--node".to_string(),
        target.rpc_url.to_string(),
        "--chain-id".to_string(),
        target.chain_id.to_string(),
        "--output".to_string(),
        "json".to_string(),
        "--yes".to_string(),
    ];
    args.extend(keyring.signing_flags());
    args.extend(gas_flags());
    interpret_broadcast(&svoted(&args)?)
}

/// The chain runs zero-fee: every coordinator account holds no balance, and the
/// vote manager that has actually created rounds holds none either. Gas is
/// still estimated rather than fixed, so a message whose cost changes does not
/// start failing for an unrelated reason.
fn gas_flags() -> Vec<String> {
    vec![
        "--gas".to_string(),
        "auto".to_string(),
        "--gas-adjustment".to_string(),
        "1.4".to_string(),
        "--fees".to_string(),
        "0usvote".to_string(),
    ]
}

/// Reads a broadcast response, failing on a non-zero chain code.
///
/// A `sync` broadcast returns before the transaction is included, so a zero
/// code here means "accepted into the mempool", not "applied". The caller
/// confirms by polling the chain, which is the same rule the SDK's own
/// submission lifecycle follows.
fn interpret_broadcast(output: &str) -> Result<ProposalOutcome> {
    let parsed: serde_json::Value =
        serde_json::from_str(output.trim()).context("svoted tx did not return JSON")?;
    let code = parsed.get("code").and_then(serde_json::Value::as_u64);
    if code.unwrap_or_default() != 0 {
        bail!(
            "chain rejected the transaction with code {}: {}",
            code.unwrap_or_default(),
            parsed
                .get("raw_log")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("no log")
        );
    }
    let transaction_hash = parsed
        .get("txhash")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(ProposalOutcome::Pending { transaction_hash })
}

fn svoted(args: &[String]) -> Result<String> {
    let output = Command::new("svoted")
        .args(args)
        .output()
        .context("running svoted; is it on PATH?")?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        bail!(
            "svoted failed: {} {}",
            String::from_utf8_lossy(&output.stderr).trim(),
            stdout.trim()
        );
    }
    Ok(stdout)
}
