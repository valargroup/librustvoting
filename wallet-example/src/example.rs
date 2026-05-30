use std::sync::Arc;

use anyhow::{Context, Result};
use zcash_protocol::consensus::Parameters;
use zcash_voting::delegate::ResolveDelegationLwdParams;
use zcash_voting::prelude::{
    bundle_notes_for_index, delegation_submission, display_memo, gather_delegation_lwd_inputs,
    gather_delegation_wallet_inputs, precompute_delegation, raw_bundle_weight, redact_for_signer,
    setup_delegation, spend_auth_signature, take_prepared_setup, DelegationSigner,
    DelegationSubmission, GatherDelegationWalletParams, KeystoneSigningRequest, NoopCancellation,
    NoopProgressReporter, PrecomputeDelegationInputs, PreparedDelegationReport, VotingDb,
    VotingHotkey,
};
use zcash_voting::{HyperTransport, PirClientBlocking, VotingRoundParams};

/// Human-readable precompute stages printed by the runnable example.
pub const PRECOMPUTE_FLOW: &[&str] = &[
    "Resolve lightwalletd anchor tree state and consensus branch id for the round.",
    "Gather snapshot-eligible Orchard notes and account key material from the wallet DB.",
    "Connect to the PIR endpoint selected for the round snapshot.",
    "Build PrecomputeDelegationInputs with the full round note set and target bundle index.",
    "Call precompute_delegation to persist witnesses, PIR rows, and prepared PCZT state.",
];

/// Human-readable seed-signed delegation stages printed by the runnable example.
pub const DELEGATION_FLOW: &[&str] = &[
    "Re-resolve lightwalletd and wallet inputs for the selected delegation bundle.",
    "Reuse the prepared governance PCZT, or build one if the warm cache is cold.",
    "Generate the delegation proof from stored witnesses and PIR precompute rows.",
    "Sign the stored PCZT sighash with the wallet seed signer.",
    "Assemble the chain-ready DelegationSubmission for the bundle.",
];

/// Human-readable Keystone request stages printed by embedders of this example.
pub const KEYSTONE_REQUEST_FLOW: &[&str] = &[
    "Re-resolve lightwalletd and wallet inputs for the selected delegation bundle.",
    "Build and persist the governance PCZT that Keystone will sign.",
    "Redact signer-irrelevant PCZT metadata before QR or hardware transport.",
    "Build the display memo and KeystoneSigningRequest for the device.",
];

/// Human-readable Keystone submit stages printed by embedders of this example.
pub const KEYSTONE_SUBMIT_FLOW: &[&str] = &[
    "Re-resolve lightwalletd and wallet inputs for the selected delegation bundle.",
    "Generate the delegation proof from stored witnesses and PIR precompute rows.",
    "Extract the SpendAuth signature from the Keystone-signed PCZT.",
    "Assemble the chain-ready DelegationSubmission with the Keystone signature.",
];

/// Caller-owned inputs needed to warm one delegation bundle.
pub struct WalletPrecomputeRequest<'a> {
    pub account_uuid: &'a str,
    pub lightwalletd_url: &'a str,
    pub round_params: VotingRoundParams,
    pub round_name: &'a str,
    pub voting_hotkey: &'a VotingHotkey,
    pub scanned_height: u64,
    pub pir_server_url: &'a str,
    pub bundle_index: u32,
}

/// Caller-owned inputs needed to prove and seed-sign one delegation bundle.
pub struct WalletDelegateRequest<'a> {
    pub account_uuid: &'a str,
    pub lightwalletd_url: &'a str,
    pub round_params: VotingRoundParams,
    pub round_name: &'a str,
    pub voting_hotkey: &'a VotingHotkey,
    pub scanned_height: u64,
    pub pir_server_url: &'a str,
    pub bundle_index: u32,
    pub seed: &'a [u8],
}

/// Caller-owned inputs needed to build one Keystone signing request.
pub struct WalletKeystoneRequestRequest<'a> {
    pub account_uuid: &'a str,
    pub lightwalletd_url: &'a str,
    pub round_params: VotingRoundParams,
    pub round_name: &'a str,
    pub voting_hotkey: &'a VotingHotkey,
    pub scanned_height: u64,
    pub pir_server_url: &'a str,
    pub bundle_index: u32,
}

/// Caller-owned inputs needed to prove and submit one Keystone-signed bundle.
pub struct WalletKeystoneSubmitRequest<'a> {
    pub account_uuid: &'a str,
    pub lightwalletd_url: &'a str,
    pub round_params: VotingRoundParams,
    pub round_name: &'a str,
    pub voting_hotkey: &'a VotingHotkey,
    pub scanned_height: u64,
    pub pir_server_url: &'a str,
    pub bundle_index: u32,
    pub signing_request: &'a KeystoneSigningRequest,
    pub signed_pczt_bytes: &'a [u8],
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
        network: request.voting_hotkey.network(),
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
        voting_hotkey: request.voting_hotkey,
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
        cancellation: &cancellation,
    };

    // 5. Warm persistent artifacts used by the later delegation proof step:
    // round rows, note witnesses, PIR rows, and the prepared governance PCZT.
    precompute_delegation(voting_db, wallet_db, inputs, &pir_client, &progress)
        .context("precompute delegation bundle")
}

/// Example wallet-side orchestration for proving and seed-signing one bundle.
///
/// This continues from precomputed voting DB state: witnesses, PIR rows, and the
/// governance PCZT should already be warmed for the target bundle.
pub async fn prove_and_submit_delegation_bundle<C, P, CL, R>(
    voting_db: &VotingDb,
    wallet_db: &zcash_client_sqlite::WalletDb<C, P, CL, R>,
    request: WalletDelegateRequest<'_>,
) -> Result<DelegationSubmission>
where
    C: std::borrow::Borrow<rusqlite::Connection>,
    P: Parameters,
{
    // 1. Re-resolve the chain and wallet inputs so this example API can be run
    // independently after a prior precompute pass.
    let cancellation = NoopCancellation;
    let lwd_inputs = gather_delegation_lwd_inputs(ResolveDelegationLwdParams {
        lightwalletd_url: request.lightwalletd_url,
        network: request.voting_hotkey.network(),
        round_params: request.round_params,
        round_name: request.round_name,
        cancellation: &cancellation,
    })
    .await
    .context("gather delegation lightwalletd inputs")?;

    let round_params = lwd_inputs.round_params;
    let round_id = round_params.vote_round_id.clone();
    let resolved_round_name = lwd_inputs.resolved_round_name;
    let anchor_tree_state_bytes = lwd_inputs.anchor_tree_state_bytes;
    let branch_id_provider = lwd_inputs.branch_id_provider;

    let wallet_inputs = gather_delegation_wallet_inputs(GatherDelegationWalletParams {
        wallet_db,
        account_uuid: request.account_uuid,
        voting_hotkey: request.voting_hotkey,
        snapshot_height: round_params.snapshot_height,
        scanned_height: request.scanned_height,
        anchor_tree_state_bytes,
        resolved_round_name,
    })
    .context("gather delegation wallet inputs")?;

    // 2. Reconstruct the bundle layout and the exact notes represented by this
    // bundle index. The existing rows are validated against the current inputs.
    let layout = voting_db
        .ensure_bundles_with_skipped_suffix(&round_id, &wallet_inputs.round_note_infos)
        .context("ensure delegation bundles")?;
    let bundle_note_infos = bundle_notes_for_index(
        &wallet_inputs.round_note_infos,
        &layout,
        request.bundle_index,
    )
    .context("derive delegation bundle notes")?;

    // 3. Consume the warmed PCZT setup if precompute inserted one. Falling back
    // keeps the example runnable when the process-local cache has expired.
    let progress = NoopProgressReporter;
    let _delegation_setup = match take_prepared_setup(
        voting_db,
        &round_id,
        request.bundle_index,
        &wallet_inputs.delegation_keys,
        &bundle_note_infos,
    )
    .context("take prepared delegation setup")?
    {
        Some(setup) => setup,
        None => setup_delegation(
            voting_db,
            &round_id,
            request.bundle_index,
            &bundle_note_infos,
            &wallet_inputs.delegation_keys,
            &branch_id_provider,
            &progress,
        )
        .context("setup delegation bundle")?,
    };

    // 4. Generate the proof using the witnesses and PIR rows warmed by
    // precompute_delegation_bundle.
    let pir_client =
        PirClientBlocking::with_transport(request.pir_server_url, Arc::new(HyperTransport::new()))
            .context("connect to PIR server")?;
    zcash_voting::delegate::prove(
        voting_db,
        &round_id,
        request.bundle_index,
        &bundle_note_infos,
        &wallet_inputs.delegation_keys,
        &pir_client,
        &progress,
    )
    .context("prove delegation bundle")?;

    // 5. Assemble chain-ready submission fields. Real wallet apps should keep
    // the seed in a secret container; this example reads raw bytes from env.
    delegation_submission(
        voting_db,
        &round_id,
        request.bundle_index,
        DelegationSigner::seed(request.seed, &wallet_inputs.delegation_keys),
    )
    .context("assemble seed-signed delegation submission")
}

/// Example wallet-side orchestration for building one Keystone signing request.
///
/// The returned `redacted_pczt_bytes` are the bytes a wallet would UR-encode for
/// Keystone. The full setup remains in Rust-side voting state and in the request
/// so the later submit step can verify the exact sighash that was signed.
pub async fn build_keystone_delegation_request<C, P, CL, R>(
    voting_db: &VotingDb,
    wallet_db: &zcash_client_sqlite::WalletDb<C, P, CL, R>,
    request: WalletKeystoneRequestRequest<'_>,
) -> Result<KeystoneSigningRequest>
where
    C: std::borrow::Borrow<rusqlite::Connection>,
    P: Parameters,
{
    // 1. Re-resolve the same round and wallet inputs that the proof step will
    // later validate against before submission.
    let cancellation = NoopCancellation;
    let lwd_inputs = gather_delegation_lwd_inputs(ResolveDelegationLwdParams {
        lightwalletd_url: request.lightwalletd_url,
        network: request.voting_hotkey.network(),
        round_params: request.round_params,
        round_name: request.round_name,
        cancellation: &cancellation,
    })
    .await
    .context("gather delegation lightwalletd inputs")?;

    let round_params = lwd_inputs.round_params;
    let round_id = round_params.vote_round_id.clone();
    let resolved_round_name = lwd_inputs.resolved_round_name;
    let display_round_name = resolved_round_name.clone();
    let anchor_tree_state_bytes = lwd_inputs.anchor_tree_state_bytes;
    let branch_id_provider = lwd_inputs.branch_id_provider;

    let wallet_inputs = gather_delegation_wallet_inputs(GatherDelegationWalletParams {
        wallet_db,
        account_uuid: request.account_uuid,
        voting_hotkey: request.voting_hotkey,
        snapshot_height: round_params.snapshot_height,
        scanned_height: request.scanned_height,
        anchor_tree_state_bytes,
        resolved_round_name,
    })
    .context("gather delegation wallet inputs")?;

    // 2. Reconstruct the bundle layout and exact notes represented by this
    // bundle before creating the signer-facing PCZT.
    let layout = voting_db
        .ensure_bundles_with_skipped_suffix(&round_id, &wallet_inputs.round_note_infos)
        .context("ensure delegation bundles")?;
    let bundle_note_infos = bundle_notes_for_index(
        &wallet_inputs.round_note_infos,
        &layout,
        request.bundle_index,
    )
    .context("derive delegation bundle notes")?;

    // 3. Build the full governance PCZT. The signer only receives the redacted
    // bytes, but the complete setup is needed for later proof/submission checks.
    let progress = NoopProgressReporter;
    let setup = setup_delegation(
        voting_db,
        &round_id,
        request.bundle_index,
        &bundle_note_infos,
        &wallet_inputs.delegation_keys,
        &branch_id_provider,
        &progress,
    )
    .context("setup Keystone delegation bundle")?;

    let redacted_pczt_bytes =
        redact_for_signer(&setup.pczt_bytes).context("redact PCZT for Keystone signer")?;
    let delegated_weight_zatoshi =
        raw_bundle_weight(&bundle_note_infos).context("calculate Keystone bundle weight")?;
    let display_memo = display_memo(&display_round_name, delegated_weight_zatoshi);

    Ok(KeystoneSigningRequest {
        setup,
        redacted_pczt_bytes,
        display_memo,
        eligible_weight_zatoshi: layout.eligible_weight,
        delegated_weight_zatoshi,
        bundle_count: layout.bundle_count,
        bundle_index: request.bundle_index,
    })
}

/// Example wallet-side orchestration for proving and submitting a Keystone bundle.
///
/// This function intentionally does not rebuild the governance PCZT. It extracts
/// Keystone's SpendAuth signature from the signed PCZT and pairs it with the
/// original setup sighash that the device was asked to sign.
pub async fn prove_and_submit_keystone_delegation_bundle<C, P, CL, R>(
    voting_db: &VotingDb,
    wallet_db: &zcash_client_sqlite::WalletDb<C, P, CL, R>,
    request: WalletKeystoneSubmitRequest<'_>,
) -> Result<DelegationSubmission>
where
    C: std::borrow::Borrow<rusqlite::Connection>,
    P: Parameters,
{
    // 1. Re-resolve chain and wallet inputs so the proof is generated against
    // the same bundle layout represented by the signed PCZT request.
    let cancellation = NoopCancellation;
    let lwd_inputs = gather_delegation_lwd_inputs(ResolveDelegationLwdParams {
        lightwalletd_url: request.lightwalletd_url,
        network: request.voting_hotkey.network(),
        round_params: request.round_params,
        round_name: request.round_name,
        cancellation: &cancellation,
    })
    .await
    .context("gather delegation lightwalletd inputs")?;

    let round_params = lwd_inputs.round_params;
    let round_id = round_params.vote_round_id.clone();
    let resolved_round_name = lwd_inputs.resolved_round_name;
    let anchor_tree_state_bytes = lwd_inputs.anchor_tree_state_bytes;

    let wallet_inputs = gather_delegation_wallet_inputs(GatherDelegationWalletParams {
        wallet_db,
        account_uuid: request.account_uuid,
        voting_hotkey: request.voting_hotkey,
        snapshot_height: round_params.snapshot_height,
        scanned_height: request.scanned_height,
        anchor_tree_state_bytes,
        resolved_round_name,
    })
    .context("gather delegation wallet inputs")?;

    let layout = voting_db
        .ensure_bundles_with_skipped_suffix(&round_id, &wallet_inputs.round_note_infos)
        .context("ensure delegation bundles")?;
    let bundle_note_infos = bundle_notes_for_index(
        &wallet_inputs.round_note_infos,
        &layout,
        request.bundle_index,
    )
    .context("derive delegation bundle notes")?;

    // 2. Generate the proof using warmed witnesses and PIR rows, without
    // rebuilding the PCZT that Keystone already signed.
    let progress = NoopProgressReporter;
    let pir_client =
        PirClientBlocking::with_transport(request.pir_server_url, Arc::new(HyperTransport::new()))
            .context("connect to PIR server")?;
    zcash_voting::delegate::prove(
        voting_db,
        &round_id,
        request.bundle_index,
        &bundle_note_infos,
        &wallet_inputs.delegation_keys,
        &pir_client,
        &progress,
    )
    .context("prove delegation bundle")?;

    // 3. Extract the SpendAuth signature Keystone inserted into the PCZT and
    // assemble the final chain-ready submission with the original setup sighash.
    let sig = spend_auth_signature(
        request.signed_pczt_bytes,
        request.signing_request.setup.action_index,
    )
    .context("extract Keystone SpendAuth signature")?;
    let sighash = request.signing_request.setup.pczt_sighash;

    delegation_submission(
        voting_db,
        &round_id,
        request.bundle_index,
        DelegationSigner::Keystone { sig, sighash },
    )
    .context("assemble Keystone-signed delegation submission")
}
