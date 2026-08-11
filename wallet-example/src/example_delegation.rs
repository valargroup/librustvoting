use std::sync::Arc;

use anyhow::{Context, Result};
use ff::PrimeField;
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_protocol::consensus::Parameters;
use zcash_voting::delegate::ResolveDelegationLwdParams;
use zcash_voting::prelude::{
    export_delegation_capability, gather_delegation_lwd_inputs, import_delegation_capability,
    prepare_delegation_bundle as prepare_bundle_state, spend_auth_signature,
    DelegationSigningRequest, DelegationSubmission, ImportDelegationCapabilityParams,
    KeystoneSigningRequest, Network, NoopProgressReporter, PrepareDelegationBundleParams,
    PreparedDelegationBundle, PreparedDelegationReport, PreparedSigner, VotingDb, VotingHotkey,
};
use zcash_voting::wire::PirLayout;
use zcash_voting::{
    connect_pir_blocking, BundlePolicy, HyperTransport, PirClientBlocking, VotingRoundParams,
};
use zip32::{fingerprint::SeedFingerprint, AccountId};

/// Inputs for preparing one reusable delegation bundle context.
///
/// `round_params`, `round_name`, and `lightwalletd_url` identify the round and
/// chain anchor. The wallet database supplies eligible notes and account keys
/// for `account_uuid`.
pub struct PrepareRequest<'a> {
    pub account_uuid: &'a str,
    pub lightwalletd_url: &'a str,
    pub network: Network,
    pub round_params: VotingRoundParams,
    pub round_name: &'a str,
    pub voting_hotkey: &'a VotingHotkey,
    pub session_json: Option<&'a str>,
    pub bundle_index: u32,
    pub bundle_policy: BundlePolicy,
}

/// Provider-side capability result that must be durably stored before broadcast.
pub struct CustodyCapabilityExport {
    /// Exact canonical capability bytes delivered to the customer.
    pub capability_json: Vec<u8>,
    /// Lowercase SHA-256 acknowledged by the customer after import.
    pub package_digest: String,
}

/// Exports a completed provider delegation for customer handoff.
///
/// The provider retains the hotkey and therefore retains shared voting
/// authority. It must durably store `voting_hotkey.stored_secret()`, the
/// returned values, and the exact signed transaction bytes before broadcasting.
pub fn export_custody_capability(
    voting_db: &VotingDb,
    vote_chain_id: &str,
    round_id: &str,
    voting_hotkey: &VotingHotkey,
    signed_delegation_txs: &[Vec<u8>],
) -> Result<CustodyCapabilityExport> {
    let capability = export_delegation_capability(
        voting_db,
        vote_chain_id,
        round_id,
        voting_hotkey,
        signed_delegation_txs,
    )
    .context("export delegation capability")?;
    Ok(CustodyCapabilityExport {
        capability_json: capability.to_json().context("encode capability")?,
        package_digest: capability.package_digest().context("digest capability")?,
    })
}

/// Imports provider-delivered voting authority into the customer database.
///
/// The caller should store the opaque hotkey secret in platform secure storage
/// and return the digest as its exact-byte delivery acknowledgement.
pub fn import_custody_handoff(
    voting_db: &VotingDb,
    hotkey_secret: &[u8],
    capability_json: &[u8],
    expected_chain_id: &str,
    expected_network: Network,
    expected_round_params: &VotingRoundParams,
    session_json: Option<&str>,
) -> Result<(VotingHotkey, String)> {
    let voting_hotkey = VotingHotkey::from_stored_secret(hotkey_secret, expected_network)
        .context("reconstruct delivered voting hotkey")?;
    let digest = import_delegation_capability(
        voting_db,
        capability_json,
        ImportDelegationCapabilityParams {
            voting_hotkey: &voting_hotkey,
            expected_chain_id,
            expected_network,
            expected_round_params,
            session_json,
        },
    )
    .context("import delegation capability")?;
    Ok((voting_hotkey, digest))
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
    let lwd_inputs = gather_delegation_lwd_inputs(ResolveDelegationLwdParams {
        lightwalletd_url: request.lightwalletd_url,
        network: request.network,
        round_params: request.round_params,
        round_name: request.round_name,
    })
    .await
    .context("gather delegation lightwalletd inputs")?;

    prepare_bundle_state(
        voting_db,
        wallet_db,
        PrepareDelegationBundleParams {
            lwd: lwd_inputs,
            session_json: request.session_json,
            account_uuid: request.account_uuid,
            voting_hotkey: request.voting_hotkey,
            bundle_index: request.bundle_index,
            bundle_policy: request.bundle_policy,
        },
    )
    .context("prepare delegation bundle with witnesses")
}

/// Precomputes persistent artifacts needed to later prove one delegation bundle.
///
/// This stores note witnesses and PIR-backed nullifier data for the prepared
/// bundle. It does not build a PCZT, prove, sign, or submit a delegation.
///
/// `pir_layout` is the dynamic config's layout used for the PIR handshake.
/// `pir_server_url` is the selected exact-height snapshot endpoint (typically
/// after snapshot selection). Endpoint membership against the resolved config
/// is the caller's responsibility when using these example helpers.
///
/// # Errors
///
/// Returns an error if the layout handshake fails, the PIR server cannot be
/// reached, witnesses cannot be generated, or precompute state cannot be
/// persisted.
pub fn precompute_delegation_bundle<C, P, CL, R>(
    voting_db: &VotingDb,
    wallet_db: &zcash_client_sqlite::WalletDb<C, P, CL, R>,
    prepared: &PreparedDelegationBundle,
    pir_layout: PirLayout,
    pir_server_url: &str,
) -> Result<PreparedDelegationReport>
where
    C: std::borrow::Borrow<rusqlite::Connection>,
    P: Parameters,
{
    let pir_client = connect_pir(pir_layout, pir_server_url)?;
    prepared
        .precompute(voting_db, wallet_db, &pir_client)
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
    pir_layout: PirLayout,
    pir_server_url: &str,
    seed: &[u8],
) -> Result<DelegationSubmission> {
    let progress = NoopProgressReporter;
    let delegation_setup = prepared
        .setup(voting_db, &progress)
        .context("setup delegation bundle")?;
    let signing_request = prepared
        .signing_request(voting_db)
        .context("load delegation signing request")?;
    let (sig, sighash) = example_sign_delegation_request(seed, signing_request)?;

    let pir_client = connect_pir(pir_layout, pir_server_url)?;
    prepared
        .prove(voting_db, &pir_client, &progress)
        .context("prove delegation bundle")?;

    prepared
        .signed_bundle(
            voting_db,
            delegation_setup.pczt_bytes,
            PreparedSigner::signature(sig, sighash),
        )
        .map(|bundle| bundle.submission)
        .context("assemble signed delegation submission")
}

/// Builds TX1, the redacted Keystone signing transaction for one bundle.
///
/// TX1 is a consensus-shaped Zcash PCZT used to obtain a ZIP-244 SpendAuth
/// signature. It is not the vote-chain delegation submission and must never be
/// broadcast to the Zcash network. The returned request includes signer-facing
/// redacted PCZT bytes, display metadata, bundle weights, and the local setup
/// needed to verify and submit the later signed PCZT.
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
    pir_layout: PirLayout,
    pir_server_url: &str,
    keystone_request: &KeystoneSigningRequest,
    signed_pczt_bytes: &[u8],
) -> Result<DelegationSubmission> {
    // Generate the proof using warmed witnesses and PIR rows, without
    // rebuilding the PCZT that Keystone already signed.
    let progress = NoopProgressReporter;
    let pir_client = connect_pir(pir_layout, pir_server_url)?;
    prepared
        .prove(voting_db, &pir_client, &progress)
        .context("prove delegation bundle")?;

    // Pair Keystone's SpendAuth signature with the original setup sighash.
    let action_index = usize::try_from(keystone_request.action_index)
        .context("Keystone action index does not fit usize")?;
    let sig = spend_auth_signature(signed_pczt_bytes, action_index)
        .context("extract Keystone SpendAuth signature")?;
    let signer = PreparedSigner::signature_from_bytes(&sig, &keystone_request.pczt_sighash)
        .context("validate Keystone signature fields")?;

    prepared
        .signed_bundle(voting_db, Vec::new(), signer)
        .map(|bundle| bundle.submission)
        .context("assemble Keystone-signed delegation submission")
}

fn example_sign_delegation_request(
    seed: &[u8],
    request: DelegationSigningRequest,
) -> Result<([u8; 64], [u8; 32])> {
    // This is example-only signing code. Production wallets should keep their
    // own seed storage and signing boundary, then return only the signature.
    // Real wallet integrations should route to the seed identified by
    // request.seed_fingerprint before signing. This example verifies that the
    // already selected seed matches the request.
    let seed_fingerprint = SeedFingerprint::from_seed(seed)
        .ok_or_else(|| anyhow::anyhow!("wallet seed length is not valid for ZIP-32"))?;
    if seed_fingerprint.to_bytes() != request.seed_fingerprint {
        return Err(anyhow::anyhow!(
            "wallet seed fingerprint does not match delegation signing request"
        ));
    }

    let account = AccountId::try_from(request.account_index)
        .map_err(|_| anyhow::anyhow!("invalid account_index {}", request.account_index))?;
    let usk = UnifiedSpendingKey::from_seed(&request.network, seed, account)
        .context("derive account unified spending key")?;
    let sk = *usk.orchard();
    let ask = orchard::keys::SpendAuthorizingKey::from(&sk);
    let alpha = Option::<pasta_curves::pallas::Scalar>::from(
        pasta_curves::pallas::Scalar::from_repr(request.alpha),
    )
    .ok_or_else(|| anyhow::anyhow!("delegation alpha is not a valid Pallas scalar"))?;
    let rsk = ask.randomize(&alpha);
    let mut rng = rand::rngs::OsRng;
    let sig = rsk.sign(&mut rng, &request.sighash);
    Ok(((&sig).into(), request.sighash))
}

fn connect_pir(pir_layout: PirLayout, pir_server_url: &str) -> Result<PirClientBlocking> {
    connect_pir_blocking(pir_layout, pir_server_url, Arc::new(HyperTransport::new()))
        .context("connect to PIR server")
}
