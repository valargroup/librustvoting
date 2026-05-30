use std::sync::Arc;

use anyhow::{Context, Result};
use zcash_protocol::consensus::Parameters;
use zcash_voting::delegate::ResolveDelegationLwdParams;
use zcash_voting::prelude::{
    bundle_notes_for_index, delegation_pir, delegation_submission, display_memo,
    gather_delegation_lwd_inputs, gather_delegation_wallet_inputs, note_witnesses,
    raw_bundle_weight, redact_for_signer, setup_delegation, spend_auth_signature, DelegationSigner,
    DelegationSubmission, GatherDelegationWalletParams, KeystoneSigningRequest, NoopCancellation,
    NoopProgressReporter, PreparedDelegationReport, VotingDb, VotingHotkey,
};
use zcash_voting::{HyperTransport, PirClientBlocking, VotingRoundParams};

/// Inputs for precomputing one delegation bundle.
///
/// `round_params`, `round_name`, and `lightwalletd_url` identify the round and
/// chain anchor. The wallet database supplies eligible notes for `account_uuid`,
/// and `pir_server_url` supplies PIR rows for `bundle_index`.
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

/// Inputs for proving and seed-signing one delegation bundle.
///
/// The target bundle must already be precomputed. `seed` is the wallet account
/// seed used to sign the stored delegation sighash.
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

/// Inputs for creating a Keystone signing request for one delegation bundle.
///
/// The target bundle must already be precomputed. The returned request contains
/// redacted PCZT bytes for the device while retaining local setup state.
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

/// Inputs for assembling a Keystone-signed delegation submission.
///
/// `signing_request` must be the request used to produce `signed_pczt_bytes`, so
/// the extracted signature is paired with the original setup sighash.
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

/// Precomputes persistent artifacts needed to later prove one delegation bundle.
///
/// This stores round rows, note witnesses, padded-note secrets, and PIR-backed
/// nullifier data for `bundle_index`. It does not build a PCZT, prove, sign, or
/// submit a delegation.
///
/// # Errors
///
/// Returns an error if lightwalletd inputs cannot be resolved, wallet note
/// selection fails, the PIR server cannot be reached, the bundle index is
/// invalid, or precompute state cannot be persisted.
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

    // 2. Select the Orchard notes that were eligible at the round snapshot and
    // load the account material used by the later delegation signing step.
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

    // 4. Ensure durable round and bundle rows before warming bundle-specific
    // artifacts. This mirrors `precompute_delegation` but keeps resume points
    // visible for wallet SDKs.
    let round_id = round_params.vote_round_id.as_str();
    voting_db
        .ensure_round(&round_params, None)
        .context("ensure delegation round")?;
    let layout = voting_db
        .ensure_bundles_with_skipped_suffix(round_id, &wallet_inputs.round_note_infos)
        .context("ensure delegation bundles")?;
    let bundle_note_infos = bundle_notes_for_index(
        &wallet_inputs.round_note_infos,
        &layout,
        request.bundle_index,
    )
    .context("derive delegation bundle notes")?;

    // 5. Witness generation is the expensive wallet-DB step. A resumed
    // precompute pass can skip it once this bundle's witnesses are cached.
    if !voting_db
        .has_witnesses(round_id, request.bundle_index)
        .context("check cached bundle witnesses")?
    {
        note_witnesses(
            voting_db,
            round_id,
            request.bundle_index,
            &wallet_inputs.anchor_tree_state_bytes,
            &bundle_note_infos,
            wallet_db,
        )
        .context("generate bundle witnesses")?;
    }

    // 6. Warm padded-note secrets and PIR rows used by the later delegation
    // proof step.
    let report = delegation_pir(
        voting_db,
        round_id,
        request.bundle_index,
        &bundle_note_infos,
        &pir_client,
        request.voting_hotkey.network(),
    )
    .context("precompute delegation PIR")?;

    Ok(PreparedDelegationReport {
        report,
        layout,
        bundle_index: request.bundle_index,
    })
}

/// Proves one precomputed delegation bundle and signs it with the wallet seed.
///
/// The returned `DelegationSubmission` contains the chain-ready fields for the
/// selected bundle. The function expects the target bundle's witnesses,
/// padded-note secrets, and PIR rows to have been warmed by
/// `precompute_delegation_bundle`.
///
/// # Errors
///
/// Returns an error if chain or wallet inputs cannot be resolved, the bundle is
/// missing or invalid, setup/proof generation fails, PIR access fails, signing
/// fails, or submission fields cannot be assembled.
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

    // 3. Build the key-dependent PCZT now that the wallet is ready to sign.
    let progress = NoopProgressReporter;
    let _delegation_setup = setup_delegation(
        voting_db,
        &round_id,
        request.bundle_index,
        &bundle_note_infos,
        &wallet_inputs.delegation_keys,
        &branch_id_provider,
        &progress,
    )
    .context("setup delegation bundle")?;

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

/// Builds the redacted Keystone signing request for one delegation bundle.
///
/// The returned request includes signer-facing redacted PCZT bytes, display
/// metadata, bundle weights, and the local setup needed to verify and submit the
/// later signed PCZT.
///
/// # Errors
///
/// Returns an error if chain or wallet inputs cannot be resolved, the bundle is
/// missing or invalid, PCZT setup fails, redaction fails, or bundle weight cannot
/// be calculated.
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

/// Proves a bundle and assembles a submission from a Keystone-signed PCZT.
///
/// This function does not rebuild the governance PCZT. It extracts Keystone's
/// SpendAuth signature from `signed_pczt_bytes` and pairs it with the original
/// setup sighash from `signing_request`.
///
/// # Errors
///
/// Returns an error if chain or wallet inputs cannot be resolved, the bundle is
/// missing or invalid, proof generation fails, PIR access fails, the signature
/// cannot be extracted, or submission fields cannot be assembled.
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
