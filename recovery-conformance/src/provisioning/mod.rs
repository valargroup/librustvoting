//! Building the round this suite crashes its way through.
//!
//! A round is created on the vote chain by `MsgCreateVotingSession`, which the
//! chain gates behind coordinator approval. The `svoted` CLI takes that message
//! as a JSON description file, so this package's job is to produce a
//! description that is both *valid* for the chain and *shaped* for the
//! invariants: several proposals, differing option counts, and a bundle layout
//! with a bundle to spare.
//!
//! # Why the round must be recreated, and how often
//!
//! A delegation is consumed on the vote chain per round. The Zcash notes are
//! untouched — TX1 is a PCZT-only signing artifact and is never broadcast — so
//! the voter wallet never needs re-funding. It is the round that is one-shot.
//!
//! That splits the suite in two. A stage that gets a POST onto the wire, and
//! any test that drives a resumed round to quiescence, consumes its round and
//! needs a fresh one. Everything else leaves the chain untouched and can branch
//! from one provisioned round by copying the sidecar file.
//!
//! Round creation also serializes chain-wide: the chain refuses a second
//! `CreateVotingSession` while another ceremony is pending, so the mutative
//! tier cannot provision in parallel.

mod ballot;
mod chain;
mod keyring;
mod round_params;
mod snapshot;

pub use ballot::{
    suite_ballot, RoundDescription, RoundOption, RoundProposal, SnapshotAnchor,
    EXPECTED_BUNDLE_COUNT, EXPECTED_VOTER_NOTES,
};
pub use chain::{approve_action, propose_round, ChainTarget, Dispatch, ProposalOutcome};
pub use keyring::VoteManagerKeyring;
pub use round_params::{fetch_round, ChainRound};
pub use snapshot::{assert_network, published_snapshot, resolve_anchor, PirSnapshot};

/// The coordinator `VOTE_MANAGER_VOTE_SDK` controls, registered on staging.
///
/// The config attestation key is a different account and is deliberately not
/// modelled here: this suite signs its own round entries rather than relying
/// on attestations.
///
/// Checking a derived address against this is the cheapest guard there is: one
/// local computation, no chain interaction, and it fails before anything is
/// broadcast rather than as a rejected transaction. It also pins the coin type
/// — derive with the cosmos default of 118 and the mismatch shows up here
/// rather than as an authorization failure that looks like a permissions
/// problem.
pub const COORDINATOR_ADDRESS: &str = "sv1z4rawnk8ny0pzsewyzm3egdd7296fr8p20fkf8";

/// Provisions a fresh round and waits for its ceremony to confirm.
///
/// Every chain-touching crash stage needs its own round, because a delegation
/// is consumed on the vote chain and cannot be replayed. Creation also
/// serialises chain-wide — the chain refuses a second session while one
/// ceremony is pending — so this must never be called concurrently.
///
/// Returns the chain-derived round id. The id is not chosen: the chain computes
/// it by hashing the snapshot, ballot, and vote-end together, so a round is
/// identified by what it is rather than by what it was called.
///
/// Uses [`suite_ballot`]; [`provision_round_with_ballot`] is the same sequence
/// over a ballot the caller chooses.
pub async fn provision_active_round(
    keyring: &VoteManagerKeyring,
    pir_base_url: &str,
    lightwalletd_url: &str,
    network: zcash_voting::Network,
    target: &ChainTarget<'_>,
    vote_end_time: i64,
) -> anyhow::Result<String> {
    provision_round_with_ballot(
        keyring,
        pir_base_url,
        lightwalletd_url,
        network,
        target,
        vote_end_time,
        suite_ballot(),
    )
    .await
}

/// Provisions a fresh round over an arbitrary ballot and waits for its ceremony.
///
/// The body [`provision_active_round`] delegates to, with the ballot as an
/// argument rather than a constant. The conformance matrices always pass
/// [`suite_ballot`]; a benchmark that needs a wider ballot passes its own,
/// without a second copy of the create-wait-confirm sequence existing to drift
/// from this one.
///
/// `proposals` must satisfy the chain's own rules — one-based ids, two to eight
/// options each — and the SDK's `1..=50` proposal-id bound if the round is to be
/// votable. Neither is checked here; the chain rejects the first and the
/// executor's binding rejects the second.
pub async fn provision_round_with_ballot(
    keyring: &VoteManagerKeyring,
    pir_base_url: &str,
    lightwalletd_url: &str,
    network: zcash_voting::Network,
    target: &ChainTarget<'_>,
    vote_end_time: i64,
    proposals: Vec<RoundProposal>,
) -> anyhow::Result<String> {
    use anyhow::Context;

    let anchor = resolve_anchor(pir_base_url, lightwalletd_url, network).await?;
    let description = RoundDescription::new(&anchor, vote_end_time, proposals);
    let path = std::env::temp_dir().join(format!(
        "recovery-conformance-round-{}.json",
        std::process::id()
    ));
    std::fs::write(&path, description.to_json()?).context("writing the round description")?;

    let outcome = propose_round(keyring, &path, target, Dispatch::Broadcast);
    let _ = std::fs::remove_file(&path);
    let transaction = match outcome? {
        ProposalOutcome::Pending { transaction_hash }
        | ProposalOutcome::Applied { transaction_hash } => transaction_hash,
    };

    let round_id = round_id_from_transaction(target.rpc_url, &transaction).await?;

    // The ceremony takes a handful of blocks: the round is created, validators
    // contribute to the DKG, and the round becomes active once they ack. Poll
    // the round itself rather than `ActiveRound`, which returns some other
    // active round and would never name this one.
    for _ in 0..40 {
        if let Ok(round) = fetch_round(target.rpc_url, &round_id) {
            if round.is_active() {
                return Ok(round_id);
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    anyhow::bail!("round {round_id} did not become active")
}

/// Reads the round id out of the creating transaction's own event.
///
/// The chain derives the id, so this is the only place it can be learned
/// without recomputing a Poseidon hash the chain owns.
async fn round_id_from_transaction(rpc_url: &str, transaction: &str) -> anyhow::Result<String> {
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let url = format!("{}/tx?hash=0x{transaction}", rpc_url.trim_end_matches('/'));
        let Ok(output) = std::process::Command::new("curl")
            .args(["-sS", "--max-time", "20", &url])
            .output()
        else {
            continue;
        };
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
            continue;
        };
        let events = &json["result"]["tx_result"]["events"];
        let Some(events) = events.as_array() else {
            continue;
        };
        for event in events {
            if event["type"] != "create_voting_session" {
                continue;
            }
            if let Some(attributes) = event["attributes"].as_array() {
                for attribute in attributes {
                    if attribute["key"] == "vote_round_id" {
                        if let Some(value) = attribute["value"].as_str() {
                            return Ok(value.to_string());
                        }
                    }
                }
            }
        }
    }
    anyhow::bail!("transaction {transaction} never reported a round id")
}
