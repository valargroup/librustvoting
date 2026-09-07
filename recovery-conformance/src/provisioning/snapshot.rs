//! Resolving the snapshot a round is anchored to.
//!
//! `MsgCreateVotingSession` names four snapshot fields, and none of them may be
//! invented: a round whose anchor does not describe a real published snapshot
//! is one no wallet can vote in. They come from two places, and the split
//! matters — the PIR fleet is authoritative for *which* snapshot is being
//! served, and the Zcash chain is authoritative for what was true at that
//! height.
//!
//! Deriving the anchor rather than configuring it also removes a whole class of
//! silent failure. A hardcoded height goes stale the next time staging
//! re-ingests, and the resulting round would be anchored to a snapshot the PIR
//! fleet no longer serves — which surfaces during proving, long after
//! provisioning appeared to succeed.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::ballot::SnapshotAnchor;

/// What the PIR server reports about the snapshot it is serving.
///
/// Only the fields this suite needs are modelled; the endpoint returns more.
#[derive(Debug, Deserialize)]
pub struct PirSnapshot {
    /// `"test"` or `"main"`. The server's own answer to which Zcash network
    /// the deployment indexes, and the only authoritative one.
    pub zcash_network: String,
    /// Root of the nullifier incremental Merkle tree, hex.
    ///
    /// This is the round's `nullifier_imt_root`: the PIR fleet calls it the
    /// circuit root because it is the root the delegation circuit proves
    /// against.
    pub circuit_root: String,
    /// Height the snapshot was taken at.
    pub height: u64,
}

/// Reads the snapshot the PIR fleet is currently serving.
///
/// `base_url` is a PIR endpoint from the published deployment config.
pub async fn published_snapshot(base_url: &str) -> Result<PirSnapshot> {
    let url = format!("{}/root", base_url.trim_end_matches('/'));
    let body = fetch(&url).await?;
    let snapshot: PirSnapshot =
        serde_json::from_slice(&body).with_context(|| format!("parsing {url}"))?;
    Ok(snapshot)
}

/// Confirms the PIR fleet is serving the network the suite expects.
///
/// Cheap, and it catches the failure that is hardest to read: pointed at a
/// deployment for the other network, every later step still "works" and the
/// voter wallet simply has no eligible notes.
pub fn assert_network(snapshot: &PirSnapshot, expected: zcash_voting::Network) -> Result<()> {
    let expected_name = match expected {
        zcash_voting::Network::Mainnet => "main",
        zcash_voting::Network::Testnet | zcash_voting::Network::Regtest => "test",
    };
    if snapshot.zcash_network != expected_name {
        bail!(
            "PIR serves the {} network but the suite is configured for {}",
            snapshot.zcash_network,
            expected_name
        );
    }
    Ok(())
}

async fn fetch(url: &str) -> Result<Vec<u8>> {
    let output = std::process::Command::new("curl")
        .args(["-sS", "--max-time", "30", url])
        .output()
        .with_context(|| format!("fetching {url}"))?;
    if !output.status.success() {
        bail!(
            "fetching {url} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// Builds the round's snapshot anchor for the height the PIR fleet serves.
///
/// The block hash and note-commitment root are read from lightwalletd at that
/// exact height rather than at the chain tip: the anchor has to describe the
/// snapshot, and the tip moves every seventy-five seconds.
pub async fn resolve_anchor(
    pir_base_url: &str,
    lightwalletd_url: &str,
    network: zcash_voting::Network,
) -> Result<SnapshotAnchor> {
    let snapshot = published_snapshot(pir_base_url).await?;
    assert_network(&snapshot, network)?;

    let mut client = zcash_voting::lwd::open_channel(lightwalletd_url)
        .await
        .map_err(|error| anyhow::anyhow!("opening lightwalletd channel: {error}"))?;
    let tree_state = zcash_voting::lwd::get_tree_state(&mut client, snapshot.height)
        .await
        .map_err(|error| anyhow::anyhow!("fetching tree state: {error}"))?;

    // Derived inline because the lightwalletd `TreeState` type is crate-private
    // in `zcash_voting`; this is the same derivation the wallet performs when
    // it checks a round's `nc_root` against the tree state it fetched, so a
    // round provisioned here validates under the wallet's own rule rather than
    // a second, parallel one.
    let tree = tree_state
        .ironwood_tree()
        .map_err(|error| anyhow::anyhow!("parsing ironwood tree: {error}"))?;
    let nc_root: String = tree
        .root()
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();

    Ok(SnapshotAnchor {
        height: snapshot.height,
        blockhash_hex: tree_state.hash.clone(),
        nullifier_imt_root_hex: snapshot.circuit_root,
        nc_root_hex: nc_root,
    })
}
