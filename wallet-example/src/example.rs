use std::sync::Arc;

use anyhow::{Context, Result};
use zcash_voting::delegate::ResolveDelegationLwdParams;
use zcash_voting::prelude::*;
use zcash_voting::{HyperTransport, PirClientBlocking, VotingRoundParams};

/// Human-readable precompute stages printed by the runnable example.
pub const PRECOMPUTE_FLOW: &[&str] = &[
    "Resolve lightwalletd anchor tree state and consensus branch id for the round.",
    "Gather snapshot-eligible Orchard notes and account key material from the wallet DB.",
    "Connect to the PIR endpoint selected for the round snapshot.",
    "Build PrecomputeDelegationInputs with the full round note set and target bundle index.",
    "Call precompute_delegation to persist witnesses, PIR rows, and prepared PCZT state.",
];

/// Caller-owned inputs needed to warm one delegation bundle.
pub struct WalletPrecomputeRequest<'a> {
    pub account_uuid: &'a str,
    pub lightwalletd_url: &'a str,
    pub round_params: VotingRoundParams,
    pub round_name: &'a str,
    pub hotkey_raw_address: Vec<u8>,
    pub scanned_height: u64,
    pub network: Network,
    pub pir_server_url: &'a str,
    pub bundle_index: u32,
}

/// Example wallet-side orchestration for warming one delegation bundle.
///
/// Lightwalletd supplies the round anchor and branch id. The wallet DB supplies
/// the account notes, account keys, and the local fully-scanned height.
pub async fn precompute_delegation_bundle<C, P, CL, R>(
    voting_db: &VotingDb,
    wallet_db: &zcash_client_sqlite::WalletDb<C, P, CL, R>,
    request: WalletPrecomputeRequest<'_>,
) -> Result<PreparedDelegationReport>
where
    C: std::borrow::Borrow<rusqlite::Connection>,
    P: zcash_protocol::consensus::Parameters,
{
    // 1. Resolve chain-derived inputs once, before touching voting DB state.
    let cancellation = NoopCancellation;
    let lwd_inputs = gather_delegation_lwd_inputs(ResolveDelegationLwdParams {
        lightwalletd_url: request.lightwalletd_url,
        network: request.network,
        round_params: request.round_params,
        round_name: request.round_name,
        cancellation: &cancellation,
    })
    .await
    .context("gather delegation lightwalletd inputs")?;

    let round_params = lwd_inputs.round_params;
    let resolved_round_name = lwd_inputs.resolved_round_name;
    let anchor_tree_state_bytes = lwd_inputs.anchor_tree_state_bytes;
    let branch_id_provider = lwd_inputs.branch_id_provider;

    // 2. Select the Orchard notes that were eligible at the round snapshot and
    // load the account key material needed to build the governance PCZT.
    let wallet_inputs = gather_delegation_wallet_inputs(GatherDelegationWalletParams {
        wallet_db,
        account_uuid: request.account_uuid,
        hotkey_raw_address: request.hotkey_raw_address,
        snapshot_height: round_params.snapshot_height,
        scanned_height: request.scanned_height,
        anchor_tree_state_bytes,
        resolved_round_name,
    })
    .context("gather delegation wallet inputs")?;

    // 3. Connect to the PIR endpoint chosen for this round snapshot.
    let pir_client =
        PirClientBlocking::with_transport(request.pir_server_url, Arc::new(HyperTransport::new()))
            .context("connect to PIR server")?;

    // 4. Build the high-level precompute request. Pass the full round note set;
    // the crate will create all bundles and then warm only `bundle_index`.
    let progress = NoopProgressReporter;
    let inputs = PrecomputeDelegationInputs {
        round_params: &round_params,
        session_json: None,
        bundle_index: request.bundle_index,
        round_note_infos: &wallet_inputs.round_note_infos,
        anchor_tree_state_bytes: &wallet_inputs.anchor_tree_state_bytes,
        keys: &wallet_inputs.delegation_keys,
        branch_id_provider: &branch_id_provider,
        network: request.network,
        cancellation: &cancellation,
    };

    // 5. Warm persistent artifacts used by the later delegation proof step:
    // round rows, note witnesses, PIR rows, and the prepared governance PCZT.
    precompute_delegation(voting_db, wallet_db, inputs, &pir_client, &progress)
        .context("precompute delegation bundle")
}
