//! Delegation lifecycle API.
//!
//! The functions in this module are the stable wallet-facing delegation flow:
//! build a governance PCZT, precompute proof inputs, prove delegation, assemble
//! chain submission data, and record chain recovery data.

pub use crate::phases::DelegationPhase;

use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

pub use crate::lwd::branch_id_for_height;
pub use crate::selection::{
    gather_delegation_wallet_inputs, DelegationWalletInputs, GatherDelegationWalletParams,
};
use crate::{
    governance::BUNDLE_NOTE_SLOTS,
    precompute::PirPrecomputeReport,
    round::{BundleLayout, VotingDb},
    types::{DelegationProgressReporter, Network, NoteInfo, VotingError},
};

#[cfg(feature = "pir")]
pub use crate::precompute::{precompute_delegation, precompute_delegation_with_policy};
use zcash_client_backend::data_api::{Account, WalletRead};
use zcash_client_sqlite::{AccountUuid, WalletDb};
use zcash_protocol::consensus::Parameters;

/// Wallet-derived keys and chain parameters needed to build a delegation PCZT.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DelegationKeys {
    /// Orchard full viewing key bytes for the delegating account.
    pub fvk_bytes: Vec<u8>,
    /// Raw Orchard address bytes for the hotkey output target.
    pub hotkey_raw_address: [u8; 43],
    /// ZIP-32 seed fingerprint for the account that owns the delegated notes.
    pub seed_fingerprint: [u8; 32],
    /// ZIP-32 account index used to derive account-scoped signing keys.
    pub account_index: u32,
    /// Address index used for governance PCZT output metadata.
    pub address_index: u32,
    /// SLIP-44 coin type for the target Zcash network.
    pub coin_type: u32,
    /// Human-readable round name embedded in PCZT metadata.
    pub round_name: String,
}

impl DelegationKeys {
    /// Builds delegation keys while validating the raw Orchard hotkey address.
    ///
    /// The returned value is suitable for [`setup`] and prepared-PCZT cache keys.
    ///
    /// # Errors
    ///
    /// Returns [`VotingError::InvalidInput`] when `hotkey_raw_address` is not
    /// exactly 43 bytes.
    #[allow(clippy::too_many_arguments)]
    pub fn with_hotkey_bytes(
        fvk_bytes: Vec<u8>,
        hotkey_raw_address: &[u8],
        seed_fingerprint: [u8; 32],
        account_index: u32,
        address_index: u32,
        coin_type: u32,
        round_name: String,
    ) -> Result<Self, VotingError> {
        Ok(Self {
            fvk_bytes,
            hotkey_raw_address: array43("hotkey_raw_address", hotkey_raw_address)?,
            seed_fingerprint,
            account_index,
            address_index,
            coin_type,
            round_name,
        })
    }
}

/// Delegation workflow progress events.
///
/// Host apps emit the bookend variants while orchestrating note selection,
/// signing, and payload assembly. Library functions emit the PCZT/proof stages.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum DelegationProgress {
    SelectingNotes,
    PcztBuilding,
    PcztBuilt,
    ProofStarting,
    ProofProgress(f64),
    ProofComplete,
    SigningPayload,
    PayloadReady,
}

/// Deprecated alias retained for existing SDK integrations.
#[deprecated(note = "use DelegationProgress")]
pub type DelegationStage = DelegationProgress;

/// Resolves the consensus branch id for the wallet's selected chain tip.
pub trait BranchIdProvider {
    fn consensus_branch_id(&self) -> Result<u32, VotingError>;
}

/// Branch-id provider backed by a lightwalletd tip lookup.
#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
#[derive(Clone, Debug)]
pub struct LightwalletdBranchIdProvider {
    resolved: u32,
}

#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
impl LightwalletdBranchIdProvider {
    /// Fetches the current lightwalletd tip and resolves the active branch id.
    pub async fn resolve(lightwalletd_url: &str, network: Network) -> Result<Self, VotingError> {
        let branch_height = crate::lwd::latest_block_height_with_retry(lightwalletd_url).await?;
        let resolved = crate::lwd::branch_id_for_height(network, branch_height)?;
        Ok(Self { resolved })
    }

    /// Builds a provider from an already-resolved branch id.
    pub fn resolved(consensus_branch_id: u32) -> Self {
        Self {
            resolved: consensus_branch_id,
        }
    }
}

#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
impl BranchIdProvider for LightwalletdBranchIdProvider {
    fn consensus_branch_id(&self) -> Result<u32, VotingError> {
        Ok(self.resolved)
    }
}

/// Wallet account metadata required to build delegation PCZTs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationAccountKeys {
    /// ZIP-32 account index of the loaded wallet account.
    pub account_index: u32,
    /// Orchard full viewing key bytes for the loaded account.
    pub orchard_fvk_bytes: [u8; 96],
    /// ZIP-32 seed fingerprint of the loaded account.
    pub seed_fingerprint: [u8; 32],
}

/// Delegation bundle state assembled by wallet integrations before signing.
#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
pub struct DelegationBundleContext {
    pub voting_db: VotingDb,
    pub round_id: String,
    pub bundle_index: u32,
    pub bundle_setup: BundleLayout,
    pub selected_weight_zatoshi: u64,
    pub bundle_note_infos: Vec<NoteInfo>,
    pub delegated_weight_zatoshi: u64,
    pub account: DelegationAccountKeys,
    pub hotkey_raw_address: Vec<u8>,
    pub branch_id_provider: LightwalletdBranchIdProvider,
    pub round_name: String,
}

/// Parameters for resolving lightwalletd-derived delegation inputs.
#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
pub struct ResolveDelegationLwdParams<'a> {
    pub lightwalletd_url: &'a str,
    pub network: Network,
    pub round_params: crate::VotingRoundParams,
    pub round_name: &'a str,
    pub cancellation: &'a dyn crate::Cancellation,
}

/// Parameters for gathering delegation inputs from wallet and lightwalletd state.
#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
pub struct GatherDelegationParams<'a, N> {
    pub db_path: &'a str,
    pub lightwalletd_url: &'a str,
    pub wallet_network: N,
    pub network: Network,
    pub round_params: crate::VotingRoundParams,
    pub round_name: &'a str,
    pub account_uuid: &'a str,
    pub hotkey_raw_address: Vec<u8>,
    pub cancellation: &'a dyn crate::Cancellation,
}

/// Lightwalletd-derived inputs for delegation precompute.
#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
#[derive(Clone, Debug)]
pub struct DelegationLwdInputs {
    pub round_params: crate::VotingRoundParams,
    pub resolved_round_name: String,
    pub anchor_tree_state_bytes: Vec<u8>,
    pub branch_id_provider: LightwalletdBranchIdProvider,
}

/// Validates round params and resolves lightwalletd anchor and branch state.
#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
pub async fn gather_delegation_lwd_inputs(
    params: ResolveDelegationLwdParams<'_>,
) -> Result<DelegationLwdInputs, VotingError> {
    check_cancellation(params.cancellation)?;
    crate::validate_round_params(&params.round_params)?;
    let resolved_round_name =
        crate::round::delegation_round_name(&params.round_params, params.round_name);
    let anchor_tree_state_bytes = crate::lwd::anchor_tree_state_bytes_with_retry(
        params.lightwalletd_url,
        params.round_params.snapshot_height,
    )
    .await?;
    check_cancellation(params.cancellation)?;
    let branch_id_provider =
        LightwalletdBranchIdProvider::resolve(params.lightwalletd_url, params.network).await?;

    Ok(DelegationLwdInputs {
        round_params: params.round_params,
        resolved_round_name,
        anchor_tree_state_bytes,
        branch_id_provider,
    })
}

/// Inputs gathered from lightwalletd and the wallet before voting-DB work.
#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
#[derive(Clone, Debug)]
pub struct DelegationInputs {
    pub account_uuid: String,
    pub round_params: crate::VotingRoundParams,
    pub anchor_tree_state_bytes: Vec<u8>,
    pub round_note_infos: Vec<NoteInfo>,
    pub branch_id_provider: LightwalletdBranchIdProvider,
    pub delegation_keys: DelegationKeys,
}

/// PCZT setup output that callers hand to a signer or QR encoder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationSetup {
    pub pczt_bytes: Vec<u8>,
    pub pczt_sighash: [u8; 32],
    pub rk: [u8; 32],
    pub action_index: usize,
    pub action_bytes: Vec<u8>,
}

/// Generated delegation proof and public submission fields for one bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationProof {
    pub bytes: Vec<u8>,
    pub rk: [u8; 32],
    pub nf_signed: [u8; 32],
    pub cmx_new: [u8; 32],
    pub van_comm: [u8; 32],
    pub gov_nullifiers: [[u8; 32]; BUNDLE_NOTE_SLOTS],
}

/// Signature source used when assembling a delegation transaction payload.
pub enum DelegationSigner<'a> {
    Seed {
        seed: &'a [u8],
        network: Network,
        account_index: u32,
    },
    Keystone {
        sig: [u8; 64],
        sighash: [u8; 32],
    },
}

impl DelegationSigner<'static> {
    /// Builds a Keystone signer from raw signature and sighash bytes.
    ///
    /// # Errors
    ///
    /// Returns [`VotingError::InvalidInput`] unless `sig` is 64 bytes and
    /// `sighash` is 32 bytes.
    pub fn keystone_from_bytes(sig: &[u8], sighash: &[u8]) -> Result<Self, VotingError> {
        Ok(Self::Keystone {
            sig: array64_slice("keystone_sig", sig)?,
            sighash: array32_slice("keystone_sighash", sighash)?,
        })
    }
}

/// Chain-ready delegation transaction fields for one bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationSubmission {
    pub proof: Vec<u8>,
    pub rk: [u8; 32],
    pub nf_signed: [u8; 32],
    pub cmx_new: [u8; 32],
    pub gov_comm: [u8; 32],
    pub gov_nullifiers: [[u8; 32]; BUNDLE_NOTE_SLOTS],
    pub alpha: [u8; 32],
    pub vote_round_id: String,
    pub spend_auth_sig: [u8; 64],
    pub sighash: [u8; 32],
}

/// Signed delegation bundle plus bundle-level metadata for wallet submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedDelegationBundle {
    /// Chain-ready delegation fields produced by [`submission`].
    pub submission: DelegationSubmission,
    /// Full governance PCZT bytes that were signed for this bundle.
    pub pczt_bytes: Vec<u8>,
    /// Total eligible round weight after bundle quantization.
    pub eligible_weight_zatoshi: u64,
    /// Quantized weight represented by this bundle.
    pub delegated_weight_zatoshi: u64,
    /// Number of persisted delegation bundles for the round.
    pub bundle_count: u32,
    /// Bundle index represented by this signed payload.
    pub bundle_index: u32,
}

/// Voting PCZT request that should be signed by Keystone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeystoneSigningRequest {
    /// Full setup output persisted for later proof and submission assembly.
    pub setup: DelegationSetup,
    /// Redacted PCZT bytes safe to send to the signer role.
    pub redacted_pczt_bytes: Vec<u8>,
    /// Human-readable memo shown to the signer.
    pub display_memo: String,
    /// Total eligible round weight after bundle quantization.
    pub eligible_weight_zatoshi: u64,
    /// Quantized weight represented by this request's bundle.
    pub delegated_weight_zatoshi: u64,
    /// Number of persisted delegation bundles for the round.
    pub bundle_count: u32,
    /// Bundle index represented by this signing request.
    pub bundle_index: u32,
}

/// Result of warming delegation PIR inputs for one bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedDelegationReport {
    /// PIR rows reused from storage or fetched from the PIR server.
    pub report: PirPrecomputeReport,
    /// Current bundle layout for the round.
    pub layout: BundleLayout,
    /// Bundle index warmed by the precompute operation.
    pub bundle_index: u32,
}

/// Loads account keys needed by delegation PCZT construction from a wallet DB.
///
/// `account_uuid` must be a UUID string for an account in `db`. The account
/// must have ZIP-32 derivation metadata, a UFVK, and an Orchard component.
///
/// # Errors
///
/// Returns [`VotingError::InvalidInput`] for malformed UUIDs, missing accounts,
/// or accounts without the required derivation/viewing-key material. Wallet DB
/// read failures are returned as [`VotingError::Internal`].
pub fn load_account_keys<C, P, CL, R>(
    db: &WalletDb<C, P, CL, R>,
    account_uuid: &str,
) -> Result<DelegationAccountKeys, VotingError>
where
    C: Borrow<rusqlite::Connection>,
    P: Parameters,
{
    let account_uuid = parse_account_uuid(account_uuid)?;
    let account = db
        .get_account(account_uuid)
        .map_err(|e| VotingError::Internal {
            message: format!("failed to load voting account: {e}"),
        })?
        .ok_or_else(|| VotingError::InvalidInput {
            message: "voting account not found".to_string(),
        })?;
    let derivation =
        account
            .source()
            .key_derivation()
            .ok_or_else(|| VotingError::InvalidInput {
                message: "voting account has no ZIP-32 derivation metadata".to_string(),
            })?;
    let ufvk = account.ufvk().ok_or_else(|| VotingError::InvalidInput {
        message: "voting account has no UFVK".to_string(),
    })?;
    let orchard_fvk = ufvk.orchard().ok_or_else(|| VotingError::InvalidInput {
        message: "voting account has no Orchard viewing key".to_string(),
    })?;

    Ok(DelegationAccountKeys {
        account_index: u32::from(derivation.account_index()),
        orchard_fvk_bytes: orchard_fvk.to_bytes(),
        seed_fingerprint: derivation.seed_fingerprint().to_bytes(),
    })
}

fn parse_account_uuid(account_uuid: &str) -> Result<AccountUuid, VotingError> {
    let uuid = uuid::Uuid::parse_str(account_uuid).map_err(|e| VotingError::InvalidInput {
        message: format!("invalid account UUID: {e}"),
    })?;
    Ok(AccountUuid::from_uuid(uuid))
}

#[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
fn check_cancellation(cancellation: &dyn crate::Cancellation) -> Result<(), VotingError> {
    if cancellation.is_cancelled() {
        Err(VotingError::Cancelled)
    } else {
        Ok(())
    }
}

/// Builds and persists a governance PCZT for one bundle.
///
/// The bundle must already exist via [`VotingDb::ensure_bundles`]. The returned
/// sighash is the exact message that Keystone or the seed signer must sign.
pub fn setup(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    notes: &[NoteInfo],
    keys: &DelegationKeys,
    branch_id_provider: &dyn BranchIdProvider,
    stages: &dyn DelegationProgressReporter,
) -> Result<DelegationSetup, VotingError> {
    let consensus_branch_id = branch_id_provider.consensus_branch_id()?;
    stages.on_progress(DelegationProgress::PcztBuilding);
    let pczt = db.build_governance_pczt(
        round_id,
        bundle_index,
        notes,
        &keys.fvk_bytes,
        &keys.hotkey_raw_address,
        consensus_branch_id,
        keys.coin_type,
        &keys.seed_fingerprint,
        keys.account_index,
        &keys.round_name,
        keys.address_index,
    )?;
    stages.on_progress(DelegationProgress::PcztBuilt);

    Ok(DelegationSetup {
        pczt_bytes: pczt.pczt_bytes,
        pczt_sighash: array32("pczt_sighash", pczt.pczt_sighash)?,
        rk: array32("rk", pczt.rk)?,
        action_index: pczt.action_index,
        action_bytes: pczt.action_bytes,
    })
}

/// Generates and persists the delegation proof for one bundle.
///
/// Witnesses and PIR proof precompute data must already be present. The proof
/// result is checked against PCZT-derived public fields before persistence.
#[cfg(feature = "pir")]
pub fn prove(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    notes: &[NoteInfo],
    hotkey_raw_address: &[u8; 43],
    pir_client: &pir_client::PirClientBlocking,
    network: Network,
    stages: &dyn DelegationProgressReporter,
) -> Result<DelegationProof, VotingError> {
    stages.on_progress(DelegationProgress::ProofStarting);
    let proof = db.build_and_prove_delegation(
        round_id,
        bundle_index,
        notes,
        hotkey_raw_address,
        pir_client,
        network.id(),
        stages,
    )?;
    stages.on_progress(DelegationProgress::ProofComplete);

    Ok(DelegationProof {
        bytes: proof.proof,
        rk: array32("rk", proof.rk)?,
        nf_signed: array32("nf_signed", proof.nf_signed)?,
        cmx_new: array32("cmx_new", proof.cmx_new)?,
        van_comm: array32("van_comm", proof.van_comm)?,
        gov_nullifiers: array32x_bundle_note_slots("gov_nullifiers", proof.gov_nullifiers)?,
    })
}

/// Assembles chain-ready delegation submission fields for one bundle.
///
/// Seed signers derive the spend authorization key internally. Keystone signers
/// must provide the signature over the stored PCZT sighash.
pub fn submission(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    signer: DelegationSigner<'_>,
) -> Result<DelegationSubmission, VotingError> {
    let data = match signer {
        DelegationSigner::Seed {
            seed,
            network,
            account_index,
        } => {
            db.get_delegation_submission(round_id, bundle_index, seed, network.id(), account_index)
        }
        DelegationSigner::Keystone { sig, sighash } => {
            db.get_delegation_submission_with_keystone_sig(round_id, bundle_index, &sig, &sighash)
        }
    }?;

    Ok(DelegationSubmission {
        proof: data.proof,
        rk: array32("rk", data.rk)?,
        nf_signed: array32("nf_signed", data.nf_signed)?,
        cmx_new: array32("cmx_new", data.cmx_new)?,
        gov_comm: array32("gov_comm", data.gov_comm)?,
        gov_nullifiers: array32x_bundle_note_slots("gov_nullifiers", data.gov_nullifiers)?,
        alpha: array32("alpha", data.alpha)?,
        vote_round_id: data.vote_round_id,
        spend_auth_sig: array64("spend_auth_sig", data.spend_auth_sig)?,
        sighash: array32("sighash", data.sighash)?,
    })
}

/// Records the submitted delegation transaction hash for recovery.
pub fn record_submission(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    tx_hash: &str,
) -> Result<(), VotingError> {
    db.store_delegation_tx_hash(round_id, bundle_index, tx_hash)
}

/// Records the confirmed VAN leaf position for a delegated bundle.
pub fn record_van_position(
    db: &VotingDb,
    round_id: &str,
    bundle_index: u32,
    position: u32,
) -> Result<(), VotingError> {
    db.store_van_position(round_id, bundle_index, position)
}

/// Extracts the ZIP-244 shielded sighash from a serialized voting PCZT.
pub fn pczt_sighash(pczt_bytes: &[u8]) -> Result<[u8; 32], VotingError> {
    crate::action::extract_pczt_sighash(pczt_bytes)
}

/// Extracts one SpendAuth signature from a Keystone-signed voting PCZT.
pub fn spend_auth_signature(
    signed_pczt_bytes: &[u8],
    action_index: usize,
) -> Result<[u8; 64], VotingError> {
    crate::action::extract_spend_auth_sig(signed_pczt_bytes, action_index)
}

/// Redacts PCZT metadata that signer devices do not need.
///
/// The output preserves signing-relevant transaction data while removing
/// witness and wallet-proprietary metadata before QR or hardware transport.
///
/// # Errors
///
/// Returns [`VotingError::InvalidInput`] when `pczt_bytes` cannot be parsed.
pub fn redact_for_signer(pczt_bytes: &[u8]) -> Result<Vec<u8>, VotingError> {
    use pczt::roles::redactor::Redactor;

    let pczt = pczt::Pczt::parse(pczt_bytes).map_err(|e| VotingError::InvalidInput {
        message: format!("parse PCZT failed: {e:?}"),
    })?;

    let redacted = Redactor::new(pczt)
        .redact_global_with(|mut r| r.redact_proprietary("zcash_client_backend:proposal_info"))
        .redact_orchard_with(|mut r| {
            r.redact_actions(|mut ar| {
                ar.clear_spend_witness();
                ar.redact_output_proprietary("zcash_client_backend:output_info");
            });
        })
        .redact_sapling_with(|mut r| {
            r.redact_spends(|mut sr| sr.clear_witness());
            r.redact_outputs(|mut or| {
                or.redact_proprietary("zcash_client_backend:output_info");
            });
        })
        .redact_transparent_with(|mut r| {
            r.redact_outputs(|mut or| {
                or.redact_proprietary("zcash_client_backend:output_info");
            });
        })
        .finish();

    Ok(redacted.serialize())
}

/// Returns the human-readable Keystone memo for a delegation PCZT.
///
/// `total_weight_zatoshi` is displayed with exact 8-decimal ZEC precision. The
/// returned string is capped at 512 bytes to fit signer display constraints.
pub fn display_memo(round_name: &str, total_weight_zatoshi: u64) -> String {
    let zec_whole = total_weight_zatoshi / 100_000_000;
    let zec_frac = total_weight_zatoshi % 100_000_000;
    let memo = format!(
        "I am authorizing this hotkey managed by my wallet to vote on {} with {}.{:08} ZEC.",
        round_name, zec_whole, zec_frac
    );

    if memo.len() <= 512 {
        memo
    } else {
        String::from_utf8_lossy(&memo.as_bytes()[..512]).into_owned()
    }
}

/// Process-local cache for PCZT setup built during delegation precompute.
///
/// Entries are short-lived, consume-on-entry, and keyed by wallet id, round,
/// bundle index, note identity, and delegation keys so live signing cannot reuse
/// setup material built for different inputs.
pub mod prepared_pczt {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct PreparedSetupKey {
        wallet_id: String,
        round_id: String,
        bundle_index: u32,
        keys: DelegationKeys,
        notes: Vec<PreparedNoteKey>,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct PreparedNoteKey {
        commitment: Vec<u8>,
        nullifier: Vec<u8>,
        value: u64,
        position: u64,
        diversifier: Vec<u8>,
        rho: Vec<u8>,
        rseed: Vec<u8>,
        scope: u32,
    }

    struct PreparedSetupEntry {
        setup: DelegationSetup,
        inserted_at: Instant,
    }

    struct PreparedSetupSession {
        epoch: u64,
        updated_at: Instant,
    }

    #[derive(Default)]
    struct PreparedSetupCache {
        entries: HashMap<PreparedSetupKey, PreparedSetupEntry>,
        session_epochs: HashMap<String, PreparedSetupSession>,
    }

    const PREPARED_PCZT_TTL: Duration = Duration::from_secs(15 * 60);
    const MAX_PREPARED_PCZTS: usize = 16;
    const MAX_PREPARED_SESSIONS: usize = 32;

    static PREPARED_PCZTS: OnceLock<Mutex<PreparedSetupCache>> = OnceLock::new();

    fn cache() -> &'static Mutex<PreparedSetupCache> {
        PREPARED_PCZTS.get_or_init(|| Mutex::new(PreparedSetupCache::default()))
    }

    /// Captures the current reset epoch before cancellable precompute work.
    ///
    /// Callers pass the returned epoch to [`cache_prepared_setup`] after work
    /// completes. If the wallet cache was cleared meanwhile, insertion fails.
    ///
    /// # Errors
    ///
    /// Returns [`VotingError::Internal`] if the cache lock is poisoned.
    pub fn prepared_epoch(db: &VotingDb) -> Result<u64, VotingError> {
        let wallet_id = db.wallet_id();
        let mut cache = cache().lock().map_err(|e| VotingError::Internal {
            message: format!("prepared delegation PCZT cache lock poisoned: {e}"),
        })?;
        prune(&mut cache, Instant::now());
        Ok(current_epoch(&cache, &wallet_id))
    }

    /// Inserts prepared PCZT setup only if the wallet epoch is still current.
    ///
    /// Returns `Ok(false)` when `expected_epoch` is stale. Returns `Ok(true)`
    /// when the setup was cached and can later be consumed by
    /// [`take_prepared_setup`].
    ///
    /// # Errors
    ///
    /// Returns [`VotingError::Internal`] if the cache lock is poisoned.
    pub fn cache_prepared_setup(
        db: &VotingDb,
        round_id: &str,
        bundle_index: u32,
        keys: &DelegationKeys,
        notes: &[NoteInfo],
        expected_epoch: u64,
        setup: DelegationSetup,
    ) -> Result<bool, VotingError> {
        let wallet_id = db.wallet_id();
        let now = Instant::now();
        let mut cache = cache().lock().map_err(|e| VotingError::Internal {
            message: format!("prepared delegation PCZT cache lock poisoned: {e}"),
        })?;
        prune(&mut cache, now);
        if current_epoch(&cache, &wallet_id) != expected_epoch {
            return Ok(false);
        }

        cache.entries.insert(
            prepared_key(wallet_id, round_id, bundle_index, keys, notes),
            PreparedSetupEntry {
                setup,
                inserted_at: now,
            },
        );
        prune(&mut cache, now);
        Ok(true)
    }

    /// Removes and returns one prepared setup for exact delegation inputs.
    ///
    /// This is consume-on-entry: a second call with the same inputs returns
    /// `Ok(None)` unless another setup was cached.
    ///
    /// # Errors
    ///
    /// Returns [`VotingError::Internal`] if the cache lock is poisoned.
    pub fn take_prepared_setup(
        db: &VotingDb,
        round_id: &str,
        bundle_index: u32,
        keys: &DelegationKeys,
        notes: &[NoteInfo],
    ) -> Result<Option<DelegationSetup>, VotingError> {
        let wallet_id = db.wallet_id();
        let key = prepared_key(wallet_id, round_id, bundle_index, keys, notes);
        let mut cache = cache().lock().map_err(|e| VotingError::Internal {
            message: format!("prepared delegation PCZT cache lock poisoned: {e}"),
        })?;
        prune(&mut cache, Instant::now());
        Ok(cache.entries.remove(&key).map(|entry| entry.setup))
    }

    /// Clears prepared setups for the current wallet and advances its epoch.
    ///
    /// When `round_id` is `Some(non_empty)`, only that round's cached setups are
    /// removed. `None` and `Some("")` clear all setups for the wallet. Advancing
    /// the epoch prevents late precompute tasks from reinserting stale setup.
    ///
    /// # Errors
    ///
    /// Returns [`VotingError::Internal`] if the cache lock is poisoned.
    pub fn clear_prepared_setups(
        db: &VotingDb,
        round_id: Option<&str>,
    ) -> Result<usize, VotingError> {
        let wallet_id = db.wallet_id();
        let round_id = round_id.filter(|round_id| !round_id.is_empty());
        let mut cache = cache().lock().map_err(|e| VotingError::Internal {
            message: format!("prepared delegation PCZT cache lock poisoned: {e}"),
        })?;
        prune(&mut cache, Instant::now());
        let before = cache.entries.len();
        cache.entries.retain(|key, _| {
            key.wallet_id != wallet_id || round_id.is_some_and(|round_id| key.round_id != round_id)
        });
        let session = cache
            .session_epochs
            .entry(wallet_id)
            .or_insert(PreparedSetupSession {
                epoch: 0,
                updated_at: Instant::now(),
            });
        session.epoch = session.epoch.saturating_add(1);
        session.updated_at = Instant::now();
        Ok(before - cache.entries.len())
    }

    fn prepared_key(
        wallet_id: String,
        round_id: &str,
        bundle_index: u32,
        keys: &DelegationKeys,
        notes: &[NoteInfo],
    ) -> PreparedSetupKey {
        PreparedSetupKey {
            wallet_id,
            round_id: round_id.to_string(),
            bundle_index,
            keys: keys.clone(),
            notes: notes
                .iter()
                .map(|note| PreparedNoteKey {
                    commitment: note.commitment.clone(),
                    nullifier: note.nullifier.clone(),
                    value: note.value,
                    position: note.position,
                    diversifier: note.diversifier.clone(),
                    rho: note.rho.clone(),
                    rseed: note.rseed.clone(),
                    scope: note.scope,
                })
                .collect(),
        }
    }

    fn current_epoch(cache: &PreparedSetupCache, wallet_id: &str) -> u64 {
        cache
            .session_epochs
            .get(wallet_id)
            .map(|session| session.epoch)
            .unwrap_or(0)
    }

    fn prune(cache: &mut PreparedSetupCache, now: Instant) {
        cache
            .entries
            .retain(|_, entry| now.duration_since(entry.inserted_at) <= PREPARED_PCZT_TTL);

        while cache.entries.len() > MAX_PREPARED_PCZTS {
            let Some(oldest_key) = cache
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.inserted_at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            cache.entries.remove(&oldest_key);
        }

        let wallets_with_entries = cache
            .entries
            .keys()
            .map(|key| key.wallet_id.clone())
            .collect::<HashSet<_>>();
        cache.session_epochs.retain(|wallet_id, session| {
            now.duration_since(session.updated_at) <= PREPARED_PCZT_TTL
                || wallets_with_entries.contains(wallet_id)
        });

        while cache.session_epochs.len() > MAX_PREPARED_SESSIONS {
            let Some(oldest_key) = cache
                .session_epochs
                .iter()
                .filter(|(wallet_id, _)| !wallets_with_entries.contains(*wallet_id))
                .min_by_key(|(_, session)| session.updated_at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            cache.session_epochs.remove(&oldest_key);
        }
    }
}

pub use prepared_pczt::{
    cache_prepared_setup, clear_prepared_setups, prepared_epoch, take_prepared_setup,
};

fn array32(label: &str, value: Vec<u8>) -> Result<[u8; 32], VotingError> {
    value
        .try_into()
        .map_err(|value: Vec<u8>| VotingError::Internal {
            message: format!("{label} must be 32 bytes, got {}", value.len()),
        })
}

fn array64(label: &str, value: Vec<u8>) -> Result<[u8; 64], VotingError> {
    value
        .try_into()
        .map_err(|value: Vec<u8>| VotingError::Internal {
            message: format!("{label} must be 64 bytes, got {}", value.len()),
        })
}

fn array32x_bundle_note_slots(
    label: &str,
    values: Vec<Vec<u8>>,
) -> Result<[[u8; 32]; BUNDLE_NOTE_SLOTS], VotingError> {
    if values.len() != BUNDLE_NOTE_SLOTS {
        return Err(VotingError::Internal {
            message: format!(
                "{label} must contain {BUNDLE_NOTE_SLOTS} entries, got {}",
                values.len()
            ),
        });
    }

    let arrays = values
        .into_iter()
        .enumerate()
        .map(|(idx, value)| array32(&format!("{label}[{idx}]"), value))
        .collect::<Result<Vec<_>, _>>()?;

    arrays
        .try_into()
        .map_err(|arrays: Vec<[u8; 32]>| VotingError::Internal {
            message: format!(
                "{label} must contain {BUNDLE_NOTE_SLOTS} entries, got {}",
                arrays.len()
            ),
        })
}

fn array43(label: &str, value: &[u8]) -> Result<[u8; 43], VotingError> {
    value.try_into().map_err(|_| VotingError::InvalidInput {
        message: format!("{label} must be 43 bytes, got {}", value.len()),
    })
}

fn array32_slice(label: &str, value: &[u8]) -> Result<[u8; 32], VotingError> {
    value.try_into().map_err(|_| VotingError::InvalidInput {
        message: format!("{label} must be 32 bytes, got {}", value.len()),
    })
}

fn array64_slice(label: &str, value: &[u8]) -> Result<[u8; 64], VotingError> {
    value.try_into().map_err(|_| VotingError::InvalidInput {
        message: format!("{label} must be 64 bytes, got {}", value.len()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use orchard::{
        note::{RandomSeed, Rho},
        value::NoteValue,
    };
    use rusqlite::{params, Connection};
    use secrecy::{ExposeSecret, SecretVec};
    use zcash_client_backend::data_api::{chain::ChainState, AccountBirthday, WalletWrite};
    use zcash_client_sqlite::{util::SystemClock, wallet::init::init_wallet_db};
    use zcash_primitives::block::BlockHash;
    use zcash_protocol::consensus::{
        Network as ZcashNetwork, NetworkConstants, NetworkUpgrade, Parameters,
    };
    use zip32::Scope;

    const ROUND_ID: &str = "0101010101010101010101010101010101010101010101010101010101010101";

    fn test_db(wallet_id: &str) -> VotingDb {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(wallet_id);
        db
    }

    fn test_keys(round_name: &str) -> DelegationKeys {
        DelegationKeys::with_hotkey_bytes(
            vec![8; 96],
            &[7; 43],
            [9; 32],
            0,
            0,
            1,
            round_name.to_string(),
        )
        .unwrap()
    }

    fn test_note(position: u64) -> NoteInfo {
        NoteInfo {
            commitment: vec![1; 32],
            nullifier: vec![2; 32],
            value: crate::governance::BALLOT_DIVISOR,
            position,
            diversifier: vec![3; 11],
            rho: vec![4; 32],
            rseed: vec![5; 32],
            scope: 0,
            ufvk_str: "uviewtest".to_string(),
        }
    }

    fn test_setup() -> DelegationSetup {
        DelegationSetup {
            pczt_bytes: vec![1, 2, 3],
            pczt_sighash: [0; 32],
            rk: [1; 32],
            action_index: 0,
            action_bytes: vec![4, 5, 6],
        }
    }

    fn setup_test_account(
        conn: &mut Connection,
    ) -> (
        zcash_client_sqlite::AccountUuid,
        orchard::keys::FullViewingKey,
    ) {
        let seed = SecretVec::new(vec![7u8; 32]);
        let mut db = WalletDb::from_connection(
            conn,
            ZcashNetwork::TestNetwork,
            SystemClock,
            rand::rngs::OsRng,
        );
        init_wallet_db(&mut db, Some(SecretVec::new(seed.expose_secret().to_vec()))).unwrap();

        let sapling_height = ZcashNetwork::TestNetwork
            .activation_height(NetworkUpgrade::Sapling)
            .expect("testnet has Sapling activation");
        let birthday = AccountBirthday::from_parts(
            ChainState::empty(sapling_height - 1, BlockHash([0; 32])),
            None,
        );
        let (account_uuid, usk) = db.create_account("voter", &seed, &birthday, None).unwrap();
        let orchard_fvk = usk
            .to_unified_full_viewing_key()
            .orchard()
            .expect("test account has Orchard viewing key")
            .clone();

        (account_uuid, orchard_fvk)
    }

    fn account_internal_id(
        conn: &Connection,
        account_uuid: &zcash_client_sqlite::AccountUuid,
    ) -> i64 {
        conn.query_row(
            "SELECT id FROM accounts WHERE uuid = ?1",
            params![account_uuid.expose_uuid().as_bytes()],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn insert_transaction(conn: &Connection, txid_tag: u8, mined_height: u32) -> i64 {
        let txid = [txid_tag; 32];
        conn.execute(
            "INSERT INTO transactions (txid, mined_height, min_observed_height)
             VALUES (?1, ?2, ?3)",
            params![txid, mined_height, mined_height],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_orchard_note(
        conn: &Connection,
        account_ref: i64,
        orchard_fvk: &orchard::keys::FullViewingKey,
        note_tag: u8,
        mined_height: u32,
        value_zatoshi: u64,
        commitment_tree_position: u64,
    ) {
        let transaction_id = insert_transaction(conn, note_tag, mined_height);
        let note = test_orchard_note(orchard_fvk, note_tag, value_zatoshi);
        let nullifier = note.nullifier(orchard_fvk);

        conn.execute(
            "INSERT INTO orchard_received_notes (
                transaction_id, action_index, account_id, diversifier, value, rho, rseed,
                nf, is_change, commitment_tree_position, recipient_key_scope
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, 0)",
            params![
                transaction_id,
                i64::from(note_tag),
                account_ref,
                note.recipient().diversifier().as_array(),
                value_zatoshi,
                note.rho().to_bytes(),
                note.rseed().as_bytes(),
                nullifier.to_bytes(),
                commitment_tree_position,
            ],
        )
        .unwrap();
    }

    fn test_orchard_note(
        orchard_fvk: &orchard::keys::FullViewingKey,
        note_tag: u8,
        value_zatoshi: u64,
    ) -> orchard::Note {
        let recipient = orchard_fvk.address_at(u64::from(note_tag), Scope::External);
        let rho = rho_from_nonce(u64::from(note_tag) + 1);

        for seed_nonce in 1..10_000 {
            let mut seed = [0u8; 32];
            seed[..8].copy_from_slice(&(seed_nonce + u64::from(note_tag) * 10_000).to_le_bytes());
            if let Some(rseed) = Option::<RandomSeed>::from(RandomSeed::from_bytes(seed, &rho)) {
                if let Some(note) = Option::<orchard::Note>::from(orchard::Note::from_parts(
                    recipient,
                    NoteValue::from_raw(value_zatoshi),
                    rho,
                    rseed,
                )) {
                    return note;
                }
            }
        }

        panic!("failed to generate valid Orchard note fixture");
    }

    fn rho_from_nonce(nonce: u64) -> Rho {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&nonce.to_le_bytes());
        Option::<Rho>::from(Rho::from_bytes(&bytes))
            .expect("small integers are valid pallas base field elements")
    }

    #[test]
    fn display_memo_uses_raw_zec_precision() {
        assert_eq!(
            display_memo("Poll", 123_456_789),
            "I am authorizing this hotkey managed by my wallet to vote on Poll with 1.23456789 ZEC."
        );
    }

    #[test]
    fn display_memo_truncates_long_messages() {
        let memo = display_memo(&"A".repeat(600), crate::governance::BALLOT_DIVISOR);

        assert_eq!(memo.len(), 512);
        assert!(memo.starts_with("I am authorizing this hotkey"));
    }

    #[test]
    fn delegation_keys_validate_hotkey_address_length() {
        let err = DelegationKeys::with_hotkey_bytes(
            vec![8; 96],
            &[7; 42],
            [9; 32],
            0,
            0,
            1,
            "Demo Round".to_string(),
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("hotkey_raw_address must be 43 bytes"));
    }

    #[cfg(any(feature = "tree-sync", feature = "client-tree-sync"))]
    #[test]
    fn lightwalletd_branch_id_provider_resolved_returns_branch_id() {
        let provider = LightwalletdBranchIdProvider::resolved(0x4DEC_4DF0);

        assert_eq!(provider.consensus_branch_id().unwrap(), 0x4DEC_4DF0);
    }

    #[test]
    fn keystone_signer_validates_signature_shapes() {
        assert!(matches!(
            DelegationSigner::keystone_from_bytes(&[1; 64], &[2; 32]).unwrap(),
            DelegationSigner::Keystone { .. }
        ));

        let sig_err = match DelegationSigner::keystone_from_bytes(&[1; 63], &[2; 32]) {
            Ok(_) => panic!("short signature should be rejected"),
            Err(err) => err.to_string(),
        };
        let sighash_err = match DelegationSigner::keystone_from_bytes(&[1; 64], &[2; 31]) {
            Ok(_) => panic!("short sighash should be rejected"),
            Err(err) => err.to_string(),
        };

        assert!(sig_err.contains("keystone_sig must be 64 bytes"));
        assert!(sighash_err.contains("keystone_sighash must be 32 bytes"));
    }

    #[test]
    fn load_account_keys_rejects_malformed_uuid() {
        let db = WalletDb::from_connection(
            rusqlite::Connection::open_in_memory().unwrap(),
            zcash_protocol::consensus::Network::TestNetwork,
            zcash_client_sqlite::util::SystemClock,
            rand::rngs::OsRng,
        );
        let err = load_account_keys(&db, "not-a-uuid")
            .unwrap_err()
            .to_string();

        assert!(err.contains("invalid account UUID"));
    }

    #[test]
    fn gather_delegation_wallet_inputs_rejects_malformed_uuid() {
        let db = WalletDb::from_connection(
            rusqlite::Connection::open_in_memory().unwrap(),
            ZcashNetwork::TestNetwork,
            SystemClock,
            rand::rngs::OsRng,
        );
        let err = gather_delegation_wallet_inputs(GatherDelegationWalletParams {
            wallet_db: &db,
            account_uuid: "not-a-uuid",
            hotkey_raw_address: vec![7; 43],
            snapshot_height: 12,
            scanned_height: 12,
            anchor_tree_state_bytes: vec![0xAA, 0xBB],
            resolved_round_name: "Demo Round".to_string(),
        })
        .unwrap_err()
        .to_string();

        assert!(err.contains("invalid account UUID"));
    }

    #[test]
    fn gather_delegation_wallet_inputs_rejects_unsynced_wallet() {
        let db = WalletDb::from_connection(
            rusqlite::Connection::open_in_memory().unwrap(),
            ZcashNetwork::TestNetwork,
            SystemClock,
            rand::rngs::OsRng,
        );
        let err = gather_delegation_wallet_inputs(GatherDelegationWalletParams {
            wallet_db: &db,
            account_uuid: "550e8400-e29b-41d4-a716-446655440000",
            hotkey_raw_address: vec![7; 43],
            snapshot_height: 12,
            scanned_height: 11,
            anchor_tree_state_bytes: vec![0xAA, 0xBB],
            resolved_round_name: "Demo Round".to_string(),
        })
        .unwrap_err()
        .to_string();

        assert!(err.contains("wallet is not synced to voting snapshot height 12"));
    }

    #[test]
    fn gather_delegation_wallet_inputs_selects_sorted_notes_and_keys() {
        let mut conn = Connection::open_in_memory().unwrap();
        let (account_uuid, orchard_fvk) = setup_test_account(&mut conn);
        let account_ref = account_internal_id(&conn, &account_uuid);
        let divisor = crate::governance::BALLOT_DIVISOR;

        insert_orchard_note(&conn, account_ref, &orchard_fvk, 1, 10, divisor, 7);
        insert_orchard_note(&conn, account_ref, &orchard_fvk, 2, 10, divisor * 2, 3);
        insert_orchard_note(&conn, account_ref, &orchard_fvk, 3, 16, divisor * 3, 11);

        let db = WalletDb::from_connection(
            &conn,
            ZcashNetwork::TestNetwork,
            SystemClock,
            rand::rngs::OsRng,
        );
        let inputs = gather_delegation_wallet_inputs(GatherDelegationWalletParams {
            wallet_db: &db,
            account_uuid: &account_uuid.expose_uuid().to_string(),
            hotkey_raw_address: vec![7; 43],
            snapshot_height: 12,
            scanned_height: 12,
            anchor_tree_state_bytes: vec![0xAA, 0xBB],
            resolved_round_name: "Demo Round".to_string(),
        })
        .unwrap();

        assert_eq!(inputs.anchor_tree_state_bytes, vec![0xAA, 0xBB]);
        assert_eq!(inputs.round_note_infos.len(), 2);
        assert_eq!(inputs.round_note_infos[0].position, 3);
        assert_eq!(inputs.round_note_infos[1].position, 7);
        assert_eq!(inputs.delegation_keys.hotkey_raw_address, [7; 43]);
        assert_eq!(
            inputs.delegation_keys.coin_type,
            ZcashNetwork::TestNetwork.network_type().coin_type()
        );
        assert_eq!(inputs.delegation_keys.round_name, "Demo Round");
    }

    #[test]
    fn gather_delegation_wallet_inputs_rejects_empty_snapshot() {
        let mut conn = Connection::open_in_memory().unwrap();
        let (account_uuid, _) = setup_test_account(&mut conn);
        let db = WalletDb::from_connection(
            &conn,
            ZcashNetwork::TestNetwork,
            SystemClock,
            rand::rngs::OsRng,
        );
        let err = gather_delegation_wallet_inputs(GatherDelegationWalletParams {
            wallet_db: &db,
            account_uuid: &account_uuid.expose_uuid().to_string(),
            hotkey_raw_address: vec![7; 43],
            snapshot_height: 12,
            scanned_height: 12,
            anchor_tree_state_bytes: vec![0xAA, 0xBB],
            resolved_round_name: "Demo Round".to_string(),
        })
        .unwrap_err()
        .to_string();

        assert!(err.contains("no spendable voting notes"));
    }

    #[test]
    fn gather_delegation_wallet_inputs_validates_hotkey_length() {
        let mut conn = Connection::open_in_memory().unwrap();
        let (account_uuid, orchard_fvk) = setup_test_account(&mut conn);
        let account_ref = account_internal_id(&conn, &account_uuid);
        insert_orchard_note(
            &conn,
            account_ref,
            &orchard_fvk,
            1,
            10,
            crate::governance::BALLOT_DIVISOR,
            7,
        );

        let db = WalletDb::from_connection(
            &conn,
            ZcashNetwork::TestNetwork,
            SystemClock,
            rand::rngs::OsRng,
        );
        let err = gather_delegation_wallet_inputs(GatherDelegationWalletParams {
            wallet_db: &db,
            account_uuid: &account_uuid.expose_uuid().to_string(),
            hotkey_raw_address: vec![7; 42],
            snapshot_height: 12,
            scanned_height: 12,
            anchor_tree_state_bytes: vec![0xAA, 0xBB],
            resolved_round_name: "Demo Round".to_string(),
        })
        .unwrap_err()
        .to_string();

        assert!(err.contains("hotkey_raw_address must be 43 bytes"));
    }

    #[test]
    fn redact_for_signer_rejects_invalid_pczt_bytes() {
        let err = redact_for_signer(&[0xFF, 0x00]).unwrap_err().to_string();

        assert!(err.contains("parse PCZT failed"));
    }

    #[test]
    fn prepared_setup_cache_is_consume_on_entry() {
        let db = test_db("prepared-cache-consume");
        clear_prepared_setups(&db, None).unwrap();
        let keys = test_keys("Demo Round");
        let notes = vec![test_note(42)];
        let epoch = prepared_epoch(&db).unwrap();

        assert!(
            cache_prepared_setup(&db, ROUND_ID, 0, &keys, &notes, epoch, test_setup(),).unwrap()
        );
        assert!(take_prepared_setup(&db, ROUND_ID, 0, &keys, &notes)
            .unwrap()
            .is_some());
        assert!(take_prepared_setup(&db, ROUND_ID, 0, &keys, &notes)
            .unwrap()
            .is_none());
    }

    #[test]
    fn prepared_setup_cache_rejects_late_epoch_insert() {
        let db = test_db("prepared-cache-epoch");
        clear_prepared_setups(&db, None).unwrap();
        let keys = test_keys("Demo Round");
        let notes = vec![test_note(42)];
        let stale_epoch = prepared_epoch(&db).unwrap();
        clear_prepared_setups(&db, None).unwrap();

        assert!(
            !cache_prepared_setup(&db, ROUND_ID, 0, &keys, &notes, stale_epoch, test_setup(),)
                .unwrap()
        );
        assert!(take_prepared_setup(&db, ROUND_ID, 0, &keys, &notes)
            .unwrap()
            .is_none());
    }
}
