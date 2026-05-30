use std::sync::Arc;

use anyhow::{Context, Result};
use zcash_protocol::consensus::Parameters;
use zcash_voting::delegate::ResolveDelegationLwdParams;
use zcash_voting::prelude::{
    gather_delegation_lwd_inputs, prepare_delegation_bundle as prepare_bundle_state,
    spend_auth_signature, DelegationSubmission, KeystoneSigningRequest, NoopCancellation,
    NoopProgressReporter, PrepareDelegationBundleParams, PreparedDelegationBundle,
    PreparedDelegationReport, PreparedSigner, VotingDb, VotingHotkey,
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
/// context is plain data that can be reused by precompute, software signing, and
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
    let cancellation = NoopCancellation;
    prepared
        .precompute(voting_db, wallet_db, &pir_client, &cancellation)
        .context("precompute delegation bundle")
}

/// Proves one precomputed delegation bundle and signs it in wallet-owned code.
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
    let _delegation_setup = prepared
        .setup(voting_db, &progress)
        .context("setup delegation bundle")?;
    let signing_request = prepared
        .signing_request(voting_db)
        .context("load delegation signing request")?;
    let signer = PreparedSigner::from_wallet_seed(seed, signing_request)
        .context("sign delegation bundle")?;

    let pir_client = connect_pir(pir_server_url)?;
    prepared
        .prove(voting_db, &pir_client, &progress)
        .context("prove delegation bundle")?;

    prepared
        .signed_bundle(voting_db, _delegation_setup.pczt_bytes, signer)
        .map(|bundle| bundle.submission)
        .context("assemble signed delegation submission")
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
    prepared
        .keystone_request(voting_db, &progress)
        .context("build Keystone delegation request")
}

/// Proves a bundle and assembles a submission from a Keystone-signed PCZT.
///
/// This function does not rebuild the governance PCZT. It extracts Keystone's
/// SpendAuth signature from `signed_pczt_bytes` and pairs it with the original
/// setup sighash from `keystone_request`.
///
/// # Errors
///
/// Returns an error if proof generation fails, PIR access fails, the signature
/// cannot be extracted, or submission fields cannot be assembled.
pub fn prove_and_submit_keystone_delegation_bundle(
    voting_db: &VotingDb,
    prepared: &PreparedDelegationBundle,
    pir_server_url: &str,
    keystone_request: &KeystoneSigningRequest,
    signed_pczt_bytes: &[u8],
) -> Result<DelegationSubmission> {
    // Generate the proof using warmed witnesses and PIR rows, without
    // rebuilding the PCZT that Keystone already signed.
    let progress = NoopProgressReporter;
    let pir_client = connect_pir(pir_server_url)?;
    prepared
        .prove(voting_db, &pir_client, &progress)
        .context("prove delegation bundle")?;

    // Pair Keystone's SpendAuth signature with the original setup sighash.
    let action_index = usize::try_from(keystone_request.action_index)
        .map_err(|_| anyhow::anyhow!("action_index does not fit usize"))?;
    let sig = spend_auth_signature(signed_pczt_bytes, action_index)
        .context("extract Keystone SpendAuth signature")?;
    let signer = PreparedSigner::signature_from_bytes(&sig, &keystone_request.pczt_sighash)
        .context("validate Keystone signature fields")?;

    prepared
        .signed_bundle(voting_db, Vec::new(), signer)
        .map(|bundle| bundle.submission)
        .context("assemble Keystone-signed delegation submission")
}

fn connect_pir(pir_server_url: &str) -> Result<PirClientBlocking> {
    PirClientBlocking::with_transport(pir_server_url, Arc::new(HyperTransport::new()))
        .context("connect to PIR server")
}
