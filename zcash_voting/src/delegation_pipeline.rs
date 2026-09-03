//! Delegation as one object.
//!
//! A [`DelegationPipeline`] binds the sidecar database, a way to open the
//! wallet database, the round's lightwalletd inputs, the account, the voting
//! hotkey, and the bundle policy once. Every stage of delegation for a bundle
//! then runs from that binding: bundle setup, eligibility, PIR warm-up, proof
//! generation, external signing requests, and the final prove-and-sign that
//! produces a chain-ready [`SignedDelegationBundle`].
//!
//! Signing never brings seed material into the crate. A software wallet
//! implements [`SpendAuthSigner`] over its own seed and returns only the
//! SpendAuth signature; a Keystone wallet supplies the device signature.

use std::{
    borrow::Borrow,
    sync::{Arc, OnceLock},
};

use prost::Message as _;
use zcash_protocol::consensus::Parameters;

use crate::backend::{zcash_client_backend, zcash_client_sqlite};
use zcash_client_backend::proto::service::TreeState;
use zcash_client_sqlite::WalletDb;

use crate::{
    delegate::{
        self, DelegationLwdInputs, DelegationProgress, DelegationProofStatus,
        DelegationRoundContext, DelegationSigningRequest, KeystoneSigningRequest,
        PrepareDelegationBundleParams, PreparedDelegationBundle, PreparedDelegationReport,
        PreparedSigner, SignedDelegationBundle,
    },
    note_bundling::{BundlePolicy, MinimumVotingEligibility},
    phases::DelegationPhase,
    pir::{PirFleet, PirProofSource},
    round::{BundleLayout, VotingDb},
    selection::select_notes_with_wallet_db,
    types::{
        DelegationProgressReporter, DelegationSetupField, Network, NoopProgressReporter, NoteInfo,
        VotingError, VotingHotkey,
    },
};

/// Opens the wallet database for reads on demand.
///
/// Wallet database handles are not `Send`, so the pipeline opens one per
/// stage on the thread that needs it instead of holding one across stages.
pub trait WalletDbOpener: Send + Sync {
    type Conn: Borrow<rusqlite::Connection>;
    type Params: Parameters;
    type Clock;
    type Rng;

    /// Opens a read-capable wallet database handle.
    fn open_for_read(
        &self,
    ) -> Result<WalletDb<Self::Conn, Self::Params, Self::Clock, Self::Rng>, VotingError>;
}

/// Opens a SQLite wallet database by path with the SDK's default settings.
#[derive(Clone, Debug)]
pub struct SqliteWalletDbOpener {
    path: String,
    network: Network,
}

impl SqliteWalletDbOpener {
    pub fn new(path: impl Into<String>, network: Network) -> Self {
        Self {
            path: path.into(),
            network,
        }
    }
}

impl WalletDbOpener for SqliteWalletDbOpener {
    type Conn = rusqlite::Connection;
    type Params = Network;
    type Clock = zcash_client_sqlite::util::SystemClock;
    type Rng = voting_crypto_deps::rand::rngs::OsRng;

    fn open_for_read(
        &self,
    ) -> Result<WalletDb<Self::Conn, Self::Params, Self::Clock, Self::Rng>, VotingError> {
        let conn = rusqlite::Connection::open(&self.path)
            .map_err(|e| VotingError::from_sqlite("failed to open wallet database", &e))?;
        Ok(WalletDb::from_connection(
            conn,
            self.network,
            zcash_client_sqlite::util::SystemClock,
            voting_crypto_deps::rand::rngs::OsRng,
        ))
    }
}

/// Produces the account SpendAuth signature for a delegation.
///
/// The wallet keeps its seed; the crate hands over only the account index,
/// network, seed fingerprint, sighash, and randomizer, and receives the
/// 64-byte signature back.
pub trait SpendAuthSigner: Send + Sync {
    fn sign(&self, request: DelegationSigningRequest) -> Result<[u8; 64], VotingError>;
}

impl<F> SpendAuthSigner for F
where
    F: Fn(DelegationSigningRequest) -> Result<[u8; 64], VotingError> + Send + Sync,
{
    fn sign(&self, request: DelegationSigningRequest) -> Result<[u8; 64], VotingError> {
        self(request)
    }
}

/// Where a Keystone signature comes from.
#[derive(Clone, Debug)]
pub enum KeystoneSignatureSource {
    /// The signature stored for the bundle through
    /// `VotingDb::store_keystone_signatures_batch`.
    Stored,
    /// A signature the host holds in memory.
    Provided { sig: Vec<u8>, sighash: Vec<u8> },
}

/// Signer for one delegation bundle.
#[derive(Clone)]
pub enum DelegationSigner {
    /// A software wallet that derives and randomizes its own SpendAuth key.
    Software(Arc<dyn SpendAuthSigner>),
    /// A Keystone device signed the redacted PCZT from
    /// [`DelegationPipeline::keystone_request`].
    Keystone(KeystoneSignatureSource),
}

/// Minimum voting eligibility plus the note value the privacy trim withholds.
///
/// Both come from one bundle plan, so the reported weight and the reported
/// loss describe the same note set. The withheld value is raw note value, not
/// bundle-quantized voting weight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VotingEligibilityReport {
    pub eligibility: MinimumVotingEligibility,
    pub privacy_trim_dropped_value_zatoshi: u64,
}

/// One account's delegation work for one round.
pub struct DelegationPipeline<W: WalletDbOpener> {
    voting_db: Arc<VotingDb>,
    wallet: W,
    lwd: DelegationLwdInputs,
    session_json: Option<String>,
    account_uuid: String,
    hotkey: Option<VotingHotkey>,
    bundle_policy: BundlePolicy,
}

impl<W: WalletDbOpener> DelegationPipeline<W> {
    /// Binds the pipeline. `hotkey` may be `None` for stages that need no
    /// hotkey (bundle setup and eligibility); preparing a bundle requires it.
    ///
    /// # Errors
    ///
    /// Returns [`VotingError::InvalidInput`] when the round params are invalid
    /// or the hotkey network does not match the lightwalletd inputs.
    pub fn new(
        voting_db: Arc<VotingDb>,
        wallet: W,
        lwd: DelegationLwdInputs,
        account_uuid: &str,
        hotkey: Option<VotingHotkey>,
        bundle_policy: BundlePolicy,
        session_json: Option<&str>,
    ) -> Result<Self, VotingError> {
        crate::validate_round_params(&lwd.round_params)?;
        if let Some(hotkey) = hotkey.as_ref() {
            if hotkey.network() != lwd.network {
                return Err(VotingError::InvalidInput {
                    message: "delegation LWD network does not match voting hotkey network"
                        .to_string(),
                });
            }
        }
        if account_uuid.trim().is_empty() {
            return Err(VotingError::InvalidInput {
                message: "account_uuid must not be empty".to_string(),
            });
        }
        Ok(Self {
            voting_db,
            wallet,
            lwd,
            session_json: session_json.map(str::to_string),
            account_uuid: account_uuid.to_string(),
            hotkey,
            bundle_policy,
        })
    }

    pub fn voting_db(&self) -> &Arc<VotingDb> {
        &self.voting_db
    }

    pub fn round_id(&self) -> &str {
        &self.lwd.round_params.vote_round_id
    }

    pub fn network(&self) -> Network {
        self.lwd.network
    }

    pub fn snapshot_height(&self) -> u64 {
        self.lwd.round_params.snapshot_height
    }

    fn hotkey(&self) -> Result<&VotingHotkey, VotingError> {
        self.hotkey
            .as_ref()
            .ok_or_else(|| VotingError::InvalidInput {
                message: "this delegation stage requires the round voting hotkey".to_string(),
            })
    }

    fn anchor_tree_state(&self) -> Result<TreeState, VotingError> {
        TreeState::decode(self.lwd.anchor_tree_state_bytes.as_slice()).map_err(|e| {
            VotingError::Internal {
                message: format!("failed to decode delegation anchor tree state: {e}"),
            }
        })
    }

    /// Ensures the round row exists and returns its display context.
    pub fn ensure_round(&self) -> Result<DelegationRoundContext, VotingError> {
        delegate::ensure_round_context(
            &self.voting_db,
            self.lwd.network,
            &self.lwd.round_params,
            &self.lwd.resolved_round_name,
            self.session_json.as_deref(),
        )
    }

    /// Selects the account's voting-eligible notes at the round snapshot.
    pub fn select_notes(&self) -> Result<Vec<NoteInfo>, VotingError> {
        let wallet = self.wallet.open_for_read()?;
        let selected = select_notes_with_wallet_db(
            &wallet,
            self.lwd.network,
            &self.account_uuid,
            self.snapshot_height(),
            self.anchor_tree_state()?,
        )?;
        Ok(selected.voting_note_infos())
    }

    /// Creates or validates the round's delegation bundle rows.
    ///
    /// Existing rows are reused only when they match the current eligible
    /// note set.
    pub fn setup_bundles(&self) -> Result<BundleLayout, VotingError> {
        self.ensure_round()?;
        let notes = self.select_notes()?;
        self.voting_db
            .ensure_bundles_with_skipped_suffix_with_policy(
                self.round_id(),
                &notes,
                self.bundle_policy,
            )
    }

    /// Checks whether the account can vote without persisting anything.
    ///
    /// Once a round has a persisted plan, its stored policy is authoritative
    /// and is used instead of the pipeline's seed policy, so the preview
    /// describes the plan the round would actually derive.
    pub fn eligibility(&self) -> Result<VotingEligibilityReport, VotingError> {
        let notes = self.select_notes()?;
        let policy = self
            .voting_db
            .effective_bundle_policy(self.round_id(), self.bundle_policy)?;
        let (eligibility, plan) =
            crate::note_bundling::minimum_voting_eligibility_and_plan_for_notes(&notes, policy)?;
        Ok(VotingEligibilityReport {
            eligibility,
            privacy_trim_dropped_value_zatoshi: plan.privacy_trim.dropped_value,
        })
    }

    /// Prepares one bundle: round metadata, wallet snapshot, and witnesses.
    pub fn prepare(&self, bundle_index: u32) -> Result<PreparedDelegationBundle, VotingError> {
        let hotkey = self.hotkey()?;
        let wallet = self.wallet.open_for_read()?;
        delegate::prepare_delegation_bundle(
            &self.voting_db,
            &wallet,
            PrepareDelegationBundleParams {
                lwd: self.lwd.clone(),
                session_json: self.session_json.as_deref(),
                account_uuid: &self.account_uuid,
                voting_hotkey: hotkey,
                bundle_index,
                bundle_policy: self.bundle_policy,
            },
        )
    }

    /// Whether a durable proof already exists for the bundle.
    pub fn has_persisted_proof(&self, bundle_index: u32) -> Result<bool, VotingError> {
        let phase = self
            .voting_db
            .delegation_phase(self.round_id(), bundle_index)?;
        Ok(matches!(
            phase,
            DelegationPhase::Proved | DelegationPhase::Submitted | DelegationPhase::Confirmed
        ))
    }

    /// Builds and persists the governance PCZT, or reuses the persisted setup.
    ///
    /// Setup is write-once. When a prior attempt already persisted the
    /// sighash and effects, the stored values are kept and no PCZT bytes are
    /// returned; signing then runs against the stored sighash.
    fn ensure_setup(
        &self,
        prepared: &PreparedDelegationBundle,
        progress: &dyn DelegationProgressReporter,
    ) -> Result<Vec<u8>, VotingError> {
        match prepared.setup(&self.voting_db, progress) {
            Ok(setup) => Ok(setup.pczt_bytes),
            Err(VotingError::SetupAlreadyPersisted {
                field: DelegationSetupField::PcztSighash | DelegationSetupField::Tx1Effects,
                ..
            }) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    /// Persists witnesses and padded secrets and warms the bundle's PIR rows.
    pub fn precompute_pir(
        &self,
        bundle_index: u32,
        pir: &PirFleet,
    ) -> Result<PreparedDelegationReport, VotingError> {
        let prepared = self.prepare(bundle_index)?;
        let wallet = self.wallet.open_for_read()?;
        pir.with_failover(|session| prepared.precompute(&self.voting_db, &wallet, session))
    }

    /// Generates or reuses the bundle's durable proof without signing.
    ///
    /// A bundle whose proof is already persisted returns
    /// [`DelegationProofStatus::Reused`] without touching PIR.
    pub fn ensure_proof(
        &self,
        bundle_index: u32,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
    ) -> Result<DelegationProofStatus, VotingError> {
        if self.has_persisted_proof(bundle_index)? {
            return Ok(DelegationProofStatus::Reused);
        }
        let prepared = self.prepare(bundle_index)?;
        self.ensure_setup(&prepared, &NoopProgressReporter)?;
        self.prove_with_fleet(&prepared, pir, progress)
    }

    fn prove_with_fleet(
        &self,
        prepared: &PreparedDelegationBundle,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
    ) -> Result<DelegationProofStatus, VotingError> {
        start_proving_cache_warmup();
        let wallet = self.wallet.open_for_read()?;
        pir.with_failover(|session| {
            let source: &dyn PirProofSource = session;
            prepared.precompute(&self.voting_db, &wallet, source)?;
            prepared
                .ensure_proof(&self.voting_db, source, progress)
                .map(|completion| completion.status)
        })
    }

    /// Builds the redacted signing request for a Keystone device.
    pub fn keystone_request(
        &self,
        bundle_index: u32,
    ) -> Result<KeystoneSigningRequest, VotingError> {
        let prepared = self.prepare(bundle_index)?;
        prepared.keystone_request(&self.voting_db, &NoopProgressReporter)
    }

    /// Proves and signs one bundle, blocking the current thread.
    ///
    /// Emits the full progress sequence: `SelectingNotes`, the PCZT and
    /// proof stages, `SigningPayload`, `PayloadReady`. Software signing
    /// builds the PCZT when no proof is persisted yet and reuses persisted
    /// setup otherwise; Keystone signing never rebuilds the PCZT the device
    /// signed. Retryable PIR failures move to the next fleet endpoint while
    /// reusing the same prepared bundle.
    pub fn prove_and_sign_blocking(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
    ) -> Result<SignedDelegationBundle, VotingError> {
        progress.on_progress(DelegationProgress::SelectingNotes);
        let prepared = self.prepare(bundle_index)?;
        let proof_persisted = self.has_persisted_proof(bundle_index)?;
        let pczt_bytes = match signer {
            DelegationSigner::Software(_) if !proof_persisted => {
                self.ensure_setup(&prepared, progress)?
            }
            _ => Vec::new(),
        };
        if proof_persisted {
            progress.on_progress(DelegationProgress::ProofComplete);
        } else {
            self.prove_with_fleet(&prepared, pir, progress)?;
        }

        progress.on_progress(DelegationProgress::SigningPayload);
        let prepared_signer = match signer {
            DelegationSigner::Software(signer) => {
                let request = prepared.signing_request(&self.voting_db)?;
                let sig = signer.sign(request)?;
                PreparedSigner::signature(sig, request.sighash)
            }
            DelegationSigner::Keystone(KeystoneSignatureSource::Provided { sig, sighash }) => {
                PreparedSigner::signature_from_bytes(sig, sighash)?
            }
            DelegationSigner::Keystone(KeystoneSignatureSource::Stored) => {
                let record = self
                    .voting_db
                    .get_keystone_signatures(self.round_id())?
                    .into_iter()
                    .find(|record| record.bundle_index == bundle_index)
                    .ok_or_else(|| VotingError::InvalidInput {
                        message: format!("no stored Keystone signature for bundle {bundle_index}"),
                    })?;
                PreparedSigner::signature_from_bytes(&record.sig, &record.sighash)?
            }
        };
        let signed = prepared.signed_bundle(&self.voting_db, pczt_bytes, prepared_signer)?;
        progress.on_progress(DelegationProgress::PayloadReady);
        Ok(signed)
    }
}

impl<W: WalletDbOpener + 'static> DelegationPipeline<W> {
    /// Proves and signs one bundle on a dedicated large-stack OS thread.
    ///
    /// The thread is not cancelled when the returned future is dropped; it
    /// runs the bundle to completion so durable state is never left half
    /// written. Callers that need cancellation should decide before calling.
    pub async fn prove_and_sign(
        self: Arc<Self>,
        bundle_index: u32,
        signer: DelegationSigner,
        pir: Arc<PirFleet>,
        progress: Arc<dyn DelegationProgressReporter>,
    ) -> Result<SignedDelegationBundle, VotingError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("voting-delegation-prove".to_string())
            .stack_size(PROVING_STACK_BYTES)
            .spawn(move || {
                let result = self.prove_and_sign_blocking(bundle_index, &signer, &pir, &*progress);
                let _ = reply_tx.send(result);
            })
            .map_err(|error| VotingError::Internal {
                message: format!("failed to spawn delegation prove thread: {error}"),
            })?;
        reply_rx.await.map_err(|_| VotingError::Internal {
            message: "delegation prove thread exited without a result".to_string(),
        })?
    }
}

/// Object-safe delegation stages the round executor drives.
///
/// [`DelegationPipeline`] implements this for any wallet opener, so the
/// executor does not carry the opener's type parameter.
pub trait DelegationDriver: Send + Sync {
    /// Round the driver is bound to.
    fn round_id(&self) -> &str;

    /// Proves and signs one bundle on the calling thread.
    fn prove_and_sign_blocking(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
    ) -> Result<SignedDelegationBundle, VotingError>;

    /// Produces a fresh SpendAuth signature over the bundle's persisted
    /// sighash, for re-dispatching a delegation that is already prepared.
    fn resign_blocking(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
    ) -> Result<[u8; 64], VotingError>;
}

impl<W: WalletDbOpener> DelegationDriver for DelegationPipeline<W> {
    fn round_id(&self) -> &str {
        DelegationPipeline::round_id(self)
    }

    fn prove_and_sign_blocking(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
        pir: &PirFleet,
        progress: &dyn DelegationProgressReporter,
    ) -> Result<SignedDelegationBundle, VotingError> {
        DelegationPipeline::prove_and_sign_blocking(self, bundle_index, signer, pir, progress)
    }

    fn resign_blocking(
        &self,
        bundle_index: u32,
        signer: &DelegationSigner,
    ) -> Result<[u8; 64], VotingError> {
        let prepared = self.prepare(bundle_index)?;
        match signer {
            DelegationSigner::Software(signer) => {
                signer.sign(prepared.signing_request(&self.voting_db)?)
            }
            DelegationSigner::Keystone(KeystoneSignatureSource::Provided { sig, .. }) => sig
                .as_slice()
                .try_into()
                .map_err(|_| VotingError::InvalidInput {
                    message: format!("Keystone signature must be 64 bytes, got {}", sig.len()),
                }),
            DelegationSigner::Keystone(KeystoneSignatureSource::Stored) => {
                let record = self
                    .voting_db
                    .get_keystone_signatures(self.round_id())?
                    .into_iter()
                    .find(|record| record.bundle_index == bundle_index)
                    .ok_or_else(|| VotingError::InvalidInput {
                        message: format!("no stored Keystone signature for bundle {bundle_index}"),
                    })?;
                record
                    .sig
                    .as_slice()
                    .try_into()
                    .map_err(|_| VotingError::InvalidInput {
                        message: format!(
                            "stored Keystone signature must be 64 bytes, got {}",
                            record.sig.len()
                        ),
                    })
            }
        }
    }
}

// Matches the keygen warm-up threads in voting-circuits.
const PROVING_STACK_BYTES: usize = 64 * 1024 * 1024;

static PROVING_CACHE_WARMUP_STARTED: OnceLock<()> = OnceLock::new();

/// Starts the process-lifetime proving-key warm-up once and returns at once.
///
/// The first proof that needs keys waits on the shared cache until this
/// warm-up, or an inline cold keygen, finishes. Later calls are no-ops.
pub fn start_proving_cache_warmup() {
    if PROVING_CACHE_WARMUP_STARTED.set(()).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("voting-proving-cache-warmup".to_string())
        .stack_size(PROVING_STACK_BYTES)
        .spawn(crate::warm_proving_caches);
}
