use std::sync::Arc;

use anyhow::{Context, Result};
use zcash_protocol::consensus::Parameters;
use zcash_voting::delegate::ResolveDelegationLwdParams;
use zcash_voting::prelude::{
    delegation_pir, delegation_submission, display_memo, gather_delegation_lwd_inputs,
    note_witnesses, prepare_delegation_bundle as prepare_bundle_state, raw_bundle_weight,
    redact_for_signer, setup_delegation, spend_auth_signature, DelegationSigner,
    DelegationSubmission, KeystoneSigningRequest, NoopCancellation, NoopProgressReporter,
    PrepareDelegationBundleParams, PreparedDelegationBundle, PreparedDelegationReport, VotingDb,
    VotingHotkey,
};
use zcash_voting::{BundlePolicy, HyperTransport, PirClientBlocking, VotingRoundParams};

/// Inputs for preparing one reusable delegation bundle context.
///
/// `round_params`, `round_name`, and `lightwalletd_url` identify the round and
/// chain anchor. The wallet database supplies eligible notes and account keys
/// for `account_uuid`.
pub struct PrepareRequest<'a> {
    pub account_uuid: &'a str,
    pub lightwalletd_url: &'a str,
    pub round_params: VotingRoundParams,
    pub round_name: &'a str,
    pub voting_hotkey: &'a VotingHotkey,
    pub scanned_height: u64,
    pub bundle_index: u32,
}

/// Resolves lightwalletd and wallet inputs for later delegation operations.
///
/// This is the only example entry point that touches lightwalletd. The returned
/// context is plain data that can be reused by precompute, seed signing, and
/// Keystone flows.
///
/// # Errors
///
/// Returns an error if lightwalletd inputs cannot be resolved, wallet note
/// selection fails, the wallet is not synced to the snapshot, or bundle rows
/// cannot be created or validated.
pub async fn prepare_delegation_bundle<C, P, CL, R>(
    voting_db: &VotingDb,
    wallet_db: &zcash_client_sqlite::WalletDb<C, P, CL, R>,
    request: PrepareRequest<'_>,
) -> Result<PreparedDelegationBundle>
where
    C: std::borrow::Borrow<rusqlite::Connection>,
    P: Parameters,
{
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

    prepare_bundle_state(
        voting_db,
        lwd_inputs,
        PrepareDelegationBundleParams {
            wallet_db,
            account_uuid: request.account_uuid,
            voting_hotkey: request.voting_hotkey,
            scanned_height: request.scanned_height,
            bundle_index: request.bundle_index,
            bundle_policy: BundlePolicy::default(),
        },
    )
    .context("prepare delegation bundle")
}

/// Precomputes persistent artifacts needed to later prove one delegation bundle.
///
/// This stores note witnesses and PIR-backed nullifier data for the prepared
/// bundle. It does not build a PCZT, prove, sign, or submit a delegation.
///
/// # Errors
///
/// Returns an error if the PIR server cannot be reached, witnesses cannot be
/// generated, or precompute state cannot be persisted.
pub fn precompute_delegation_bundle<C, P, CL, R>(
    voting_db: &VotingDb,
    wallet_db: &zcash_client_sqlite::WalletDb<C, P, CL, R>,
    prepared: &PreparedDelegationBundle,
    pir_server_url: &str,
) -> Result<PreparedDelegationReport>
where
    C: std::borrow::Borrow<rusqlite::Connection>,
    P: Parameters,
{
    let pir_client = connect_pir(pir_server_url)?;

    if !voting_db
        .has_witnesses(&prepared.round_id, prepared.bundle_index)
        .context("check cached bundle witnesses")?
    {
        note_witnesses(
            voting_db,
            &prepared.round_id,
            prepared.bundle_index,
            &prepared.anchor_tree_state_bytes,
            &prepared.bundle_note_infos,
            wallet_db,
        )
        .context("generate bundle witnesses")?;
    }

    let report = delegation_pir(
        voting_db,
        &prepared.round_id,
        prepared.bundle_index,
        &prepared.bundle_note_infos,
        &pir_client,
        prepared.network,
    )
    .context("precompute delegation PIR")?;

    Ok(PreparedDelegationReport {
        report,
        layout: prepared.layout.clone(),
        bundle_index: prepared.bundle_index,
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
/// Returns an error if setup/proof generation fails, PIR access fails, signing
/// fails, or submission fields cannot be assembled.
pub fn prove_and_submit_delegation_bundle(
    voting_db: &VotingDb,
    prepared: &PreparedDelegationBundle,
    pir_server_url: &str,
    seed: &[u8],
) -> Result<DelegationSubmission> {
    let progress = NoopProgressReporter;
    let _delegation_setup = setup_delegation(
        voting_db,
        &prepared.round_id,
        prepared.bundle_index,
        &prepared.bundle_note_infos,
        &prepared.delegation_keys,
        &prepared.branch_id_provider,
        &progress,
    )
    .context("setup delegation bundle")?;

    let pir_client = connect_pir(pir_server_url)?;
    zcash_voting::delegate::prove(
        voting_db,
        &prepared.round_id,
        prepared.bundle_index,
        &prepared.bundle_note_infos,
        &prepared.delegation_keys,
        &pir_client,
        &progress,
    )
    .context("prove delegation bundle")?;

    // Real wallet apps should keep the seed in a secret container; this example
    // reads raw bytes from env.
    delegation_submission(
        voting_db,
        &prepared.round_id,
        prepared.bundle_index,
        DelegationSigner::seed(seed, &prepared.delegation_keys),
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
/// Returns an error if PCZT setup fails, redaction fails, or bundle weight
/// cannot be calculated.
pub fn build_keystone_delegation_request(
    voting_db: &VotingDb,
    prepared: &PreparedDelegationBundle,
) -> Result<KeystoneSigningRequest> {
    // Build the full governance PCZT. The signer only receives the redacted
    // bytes, but the complete setup is needed for later proof/submission checks.
    let progress = NoopProgressReporter;
    let setup = setup_delegation(
        voting_db,
        &prepared.round_id,
        prepared.bundle_index,
        &prepared.bundle_note_infos,
        &prepared.delegation_keys,
        &prepared.branch_id_provider,
        &progress,
    )
    .context("setup Keystone delegation bundle")?;

    let redacted_pczt_bytes =
        redact_for_signer(&setup.pczt_bytes).context("redact PCZT for Keystone signer")?;
    let delegated_weight_zatoshi = raw_bundle_weight(&prepared.bundle_note_infos)
        .context("calculate Keystone bundle weight")?;
    let display_memo = display_memo(&prepared.round_name, delegated_weight_zatoshi);

    Ok(KeystoneSigningRequest {
        setup,
        redacted_pczt_bytes,
        display_memo,
        eligible_weight_zatoshi: prepared.layout.eligible_weight,
        delegated_weight_zatoshi,
        bundle_count: prepared.layout.bundle_count,
        bundle_index: prepared.bundle_index,
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
/// Returns an error if proof generation fails, PIR access fails, the signature
/// cannot be extracted, or submission fields cannot be assembled.
pub fn prove_and_submit_keystone_delegation_bundle(
    voting_db: &VotingDb,
    prepared: &PreparedDelegationBundle,
    pir_server_url: &str,
    signing_request: &KeystoneSigningRequest,
    signed_pczt_bytes: &[u8],
) -> Result<DelegationSubmission> {
    // Generate the proof using warmed witnesses and PIR rows, without
    // rebuilding the PCZT that Keystone already signed.
    let progress = NoopProgressReporter;
    let pir_client = connect_pir(pir_server_url)?;
    zcash_voting::delegate::prove(
        voting_db,
        &prepared.round_id,
        prepared.bundle_index,
        &prepared.bundle_note_infos,
        &prepared.delegation_keys,
        &pir_client,
        &progress,
    )
    .context("prove delegation bundle")?;

    // Pair Keystone's SpendAuth signature with the original setup sighash.
    let sig = spend_auth_signature(signed_pczt_bytes, signing_request.setup.action_index)
        .context("extract Keystone SpendAuth signature")?;
    let sighash = signing_request.setup.pczt_sighash;

    delegation_submission(
        voting_db,
        &prepared.round_id,
        prepared.bundle_index,
        DelegationSigner::Keystone { sig, sighash },
    )
    .context("assemble Keystone-signed delegation submission")
}

fn connect_pir(pir_server_url: &str) -> Result<PirClientBlocking> {
    PirClientBlocking::with_transport(pir_server_url, Arc::new(HyperTransport::new()))
        .context("connect to PIR server")
}
