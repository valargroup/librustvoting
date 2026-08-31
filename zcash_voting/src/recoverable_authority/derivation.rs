#[allow(unused_imports)]
pub(crate) use crate::backend::orchard;

use orchard::keys::{FullViewingKey, SpendingKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;
use zip32::{AccountId, Scope};

use crate::types::{Network, RoundBoundVotingHotkeyTarget, VotingError, VotingHotkeyTarget};

/// Byte length of a version 1 per-round voting-authority root.
pub const VOTING_AUTHORITY_ROOT_LEN: usize = 64;

/// Byte length of a version 1 Keystone host master secret.
pub const KEYSTONE_MASTER_SECRET_LEN: usize = 64;

/// Globally unique ZIP-32 registered-key context for this authority scheme.
const REGISTERED_KEY_CONTEXT: &[u8] = b"valargroup.zcash_voting.authority.v1";

const AUTHORITY_CONTEXT_DOMAIN: &[u8] = b"zcash_voting/authority-context/v1";
const REGISTERED_KEY_TAG_DOMAIN: &[u8] = b"zcash_voting/software-authority-root/v1";
const MASTER_GENERATION_ID_DOMAIN: &[u8] = b"zcash_voting/keystone-master-generation-id/v1";
const MASTER_TO_ROOT_DOMAIN: &[u8] = b"zcash_voting/master-to-round-root/v1";
const ROOT_TO_HOTKEY_DOMAIN: &[u8] = b"zcash_voting/root-to-orchard-hotkey/v1";
const ROOT_BINDING_DOMAIN: &[u8] = b"zcash_voting/authority-root-binding/v1";

/// Public scope of one Keystone host voting-master generation.
///
/// A generation is never shared across a Zcash account, network, or vote
/// chain. The round identifier is deliberately absent and is added only when
/// deriving a per-round root.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VotingAuthorityScopeV1 {
    network: Network,
    account_index: u32,
    orchard_fvk_fingerprint: [u8; 32],
    vote_chain_id: String,
}

impl VotingAuthorityScopeV1 {
    /// Builds a validated public master-generation scope.
    pub fn new(
        network: Network,
        account_index: u32,
        orchard_fvk_fingerprint: [u8; 32],
        vote_chain_id: impl Into<String>,
    ) -> Result<Self, VotingError> {
        AccountId::try_from(account_index).map_err(|_| VotingError::InvalidInput {
            message: format!("invalid ZIP-32 account_index {account_index}"),
        })?;
        let vote_chain_id = vote_chain_id.into();
        crate::types::validate_vote_chain_id(&vote_chain_id)?;
        Ok(Self {
            network,
            account_index,
            orchard_fvk_fingerprint,
            vote_chain_id,
        })
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn account_index(&self) -> u32 {
        self.account_index
    }

    pub fn orchard_fvk_fingerprint(&self) -> &[u8; 32] {
        &self.orchard_fvk_fingerprint
    }

    pub fn vote_chain_id(&self) -> &str {
        &self.vote_chain_id
    }
}

/// Complete public context bound into one version 1 per-round authority root.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VotingAuthorityContextV1 {
    scope: VotingAuthorityScopeV1,
    vote_round_id: [u8; 32],
}

impl VotingAuthorityContextV1 {
    /// Builds a context from an already verified Orchard FVK fingerprint.
    ///
    /// Restoring callers must compare the fingerprint to the actual account
    /// with [`Self::validate_orchard_fvk`] before using secret authority
    /// material. New authority construction should prefer
    /// [`Self::from_orchard_fvk`].
    pub fn from_fingerprint(
        network: Network,
        account_index: u32,
        orchard_fvk_fingerprint: [u8; 32],
        vote_chain_id: impl Into<String>,
        vote_round_id: [u8; 32],
    ) -> Result<Self, VotingError> {
        crate::types::validate_vote_round_id_hex(&hex::encode(vote_round_id))?;
        Ok(Self {
            scope: VotingAuthorityScopeV1::new(
                network,
                account_index,
                orchard_fvk_fingerprint,
                vote_chain_id,
            )?,
            vote_round_id,
        })
    }

    /// Builds a context and fingerprints the validated Orchard full viewing key.
    pub fn from_orchard_fvk(
        network: Network,
        account_index: u32,
        orchard_fvk_bytes: &[u8],
        vote_chain_id: impl Into<String>,
        vote_round_id: [u8; 32],
    ) -> Result<Self, VotingError> {
        let fingerprint = orchard_fvk_fingerprint_v1(orchard_fvk_bytes)?;
        Self::from_fingerprint(
            network,
            account_index,
            fingerprint,
            vote_chain_id,
            vote_round_id,
        )
    }

    /// Checks that an Orchard FVK belongs to the account bound into this context.
    pub fn validate_orchard_fvk(&self, orchard_fvk_bytes: &[u8]) -> Result<(), VotingError> {
        let actual = orchard_fvk_fingerprint_v1(orchard_fvk_bytes)?;
        if actual.ct_eq(&self.scope.orchard_fvk_fingerprint).into() {
            Ok(())
        } else {
            Err(VotingError::InvalidInput {
                message: "Orchard full viewing key does not match voting authority context"
                    .to_string(),
            })
        }
    }

    pub fn scope(&self) -> &VotingAuthorityScopeV1 {
        &self.scope
    }

    pub fn network(&self) -> Network {
        self.scope.network
    }

    pub fn account_index(&self) -> u32 {
        self.scope.account_index
    }

    pub fn orchard_fvk_fingerprint(&self) -> &[u8; 32] {
        &self.scope.orchard_fvk_fingerprint
    }

    pub fn vote_chain_id(&self) -> &str {
        &self.scope.vote_chain_id
    }

    pub fn vote_round_id(&self) -> &[u8; 32] {
        &self.vote_round_id
    }

    /// Canonical prefix-free encoding shared by every version 1 derivation.
    pub fn canonical_transcript(&self) -> Vec<u8> {
        canonical_transcript(AUTHORITY_CONTEXT_DOMAIN, &context_fields(self))
    }
}

/// Derives the stable public fingerprint used to bind authority to an Orchard account.
pub fn orchard_fvk_fingerprint_v1(orchard_fvk_bytes: &[u8]) -> Result<[u8; 32], VotingError> {
    let bytes: [u8; 96] = orchard_fvk_bytes
        .try_into()
        .map_err(|_| VotingError::InvalidInput {
            message: format!(
                "Orchard full viewing key must be 96 bytes, got {}",
                orchard_fvk_bytes.len()
            ),
        })?;
    FullViewingKey::from_bytes(&bytes).ok_or_else(|| VotingError::InvalidInput {
        message: "invalid Orchard full viewing key".to_string(),
    })?;
    let hash = blake2b_simd::Params::new()
        .hash_length(32)
        .personal(b"ZcashOrchardFVFP")
        .hash(&bytes);
    let mut fingerprint = [0u8; 32];
    fingerprint.copy_from_slice(hash.as_bytes());
    Ok(fingerprint)
}

/// Allocated ZIP application identifier used for software-wallet derivation.
///
/// `zcash_voting` intentionally does not invent an identifier. An integration
/// must supply its allocated value, freeze it before deployment, and retain it
/// as part of the authority-source selection for every used round.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RegisteredKeyApplicationV1(u16);

impl RegisteredKeyApplicationV1 {
    pub fn new(identifier: u16) -> Self {
        Self(identifier)
    }

    pub fn identifier(self) -> u16 {
        self.0
    }
}

/// Seed-free request for a wallet-owned ZIP-32 registered-key provider.
///
/// The provider calls `zip32::registered::cryptovalue_from_subpath` with this
/// context string, its wallet seed, `application_identifier`, and the one
/// hardened element returned by [`Self::path_element`]. The seed and any account
/// spending key must never be passed back to `zcash_voting`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoftwareRegisteredKeyRequestV1 {
    application: RegisteredKeyApplicationV1,
    context: VotingAuthorityContextV1,
    tag: Vec<u8>,
}

impl SoftwareRegisteredKeyRequestV1 {
    pub fn new(application: RegisteredKeyApplicationV1, context: VotingAuthorityContextV1) -> Self {
        let tag =
            canonical_transcript(REGISTERED_KEY_TAG_DOMAIN, &[context.canonical_transcript()]);
        Self {
            application,
            context,
            tag,
        }
    }

    pub fn application_identifier(&self) -> u16 {
        self.application.identifier()
    }

    pub fn context_string(&self) -> &'static [u8] {
        REGISTERED_KEY_CONTEXT
    }

    /// Hardened child index for the request's single nonempty subpath.
    pub fn child_index(&self) -> u32 {
        self.context.account_index()
    }

    /// Returns the request's single, hardened, nonempty registered-key path.
    pub fn path_element(&self) -> zip32::registered::PathElement<'_> {
        zip32::registered::PathElement::new(
            zip32::ChildIndex::hardened(self.context.account_index()),
            &self.tag,
        )
    }

    /// Tag for the request's single nonempty subpath.
    pub fn tag(&self) -> &[u8] {
        &self.tag
    }

    pub fn authority_context(&self) -> &VotingAuthorityContextV1 {
        &self.context
    }
}

/// Root derivation source retained by the wallet's external authority state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VotingAuthorityRootSourceV1 {
    SoftwareRegisteredKey {
        application: RegisteredKeyApplicationV1,
    },
    KeystoneMasterGeneration {
        generation_id: [u8; 32],
    },
}

/// Public metadata that authenticates a retained per-round authority root.
///
/// The tag lets restore reject a mismatched root, source, or derivation context
/// without storing the root in public metadata. Encryption, rollback
/// protection, and lifecycle state remain external wallet concerns.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VotingAuthorityRootBindingV1 {
    context: VotingAuthorityContextV1,
    authority_source: VotingAuthorityRootSourceV1,
    root_verification_tag: [u8; 32],
}

impl VotingAuthorityRootBindingV1 {
    /// Binds one root to its deterministic context and derivation source.
    pub fn bind(root: &VotingAuthorityRootV1) -> Self {
        let mut binding = Self {
            context: root.context.clone(),
            authority_source: root.source,
            root_verification_tag: [0; 32],
        };
        binding.root_verification_tag = keyed_hash32(
            root.secret_bytes(),
            ROOT_BINDING_DOMAIN,
            &binding.verification_fields(),
        );
        binding
    }

    pub fn context(&self) -> &VotingAuthorityContextV1 {
        &self.context
    }

    pub fn authority_source(&self) -> VotingAuthorityRootSourceV1 {
        self.authority_source
    }

    pub fn root_verification_tag(&self) -> &[u8; 32] {
        &self.root_verification_tag
    }

    /// Serializes the public root binding with a strict versioned schema.
    pub fn to_json(&self) -> Result<String, VotingError> {
        serde_json::to_string(&VotingAuthorityRootBindingDto::from(self)).map_err(|error| {
            VotingError::Internal {
                message: format!("failed to serialize voting authority root binding: {error}"),
            }
        })
    }

    /// Parses and validates the strict version 1 public root-binding schema.
    pub fn from_json(json: &str) -> Result<Self, VotingError> {
        let dto: VotingAuthorityRootBindingDto =
            serde_json::from_str(json).map_err(|error| VotingError::InvalidInput {
                message: format!("invalid voting authority root binding JSON: {error}"),
            })?;
        dto.try_into()
    }

    fn verification_fields(&self) -> Vec<Vec<u8>> {
        let mut fields = vec![self.context.canonical_transcript()];
        match self.authority_source {
            VotingAuthorityRootSourceV1::SoftwareRegisteredKey { application } => {
                fields.push(vec![0]);
                fields.push(application.identifier().to_le_bytes().to_vec());
            }
            VotingAuthorityRootSourceV1::KeystoneMasterGeneration { generation_id } => {
                fields.push(vec![1]);
                fields.push(generation_id.to_vec());
            }
        }
        fields
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VotingAuthorityRootBindingDto {
    version: u32,
    zcash_network: String,
    account_index: u32,
    orchard_fvk_fingerprint: String,
    vote_chain_id: String,
    vote_round_id: String,
    authority_source: VotingAuthorityRootSourceDto,
    root_verification_tag: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
enum VotingAuthorityRootSourceDto {
    SoftwareRegisteredKey { application_identifier: u16 },
    KeystoneMasterGeneration { generation_id: String },
}

impl From<&VotingAuthorityRootBindingV1> for VotingAuthorityRootBindingDto {
    fn from(binding: &VotingAuthorityRootBindingV1) -> Self {
        Self {
            version: 1,
            zcash_network: network_name(binding.context.network()).to_string(),
            account_index: binding.context.account_index(),
            orchard_fvk_fingerprint: hex::encode(binding.context.orchard_fvk_fingerprint()),
            vote_chain_id: binding.context.vote_chain_id().to_string(),
            vote_round_id: hex::encode(binding.context.vote_round_id()),
            authority_source: match binding.authority_source {
                VotingAuthorityRootSourceV1::SoftwareRegisteredKey { application } => {
                    VotingAuthorityRootSourceDto::SoftwareRegisteredKey {
                        application_identifier: application.identifier(),
                    }
                }
                VotingAuthorityRootSourceV1::KeystoneMasterGeneration { generation_id } => {
                    VotingAuthorityRootSourceDto::KeystoneMasterGeneration {
                        generation_id: hex::encode(generation_id),
                    }
                }
            },
            root_verification_tag: hex::encode(binding.root_verification_tag),
        }
    }
}

impl TryFrom<VotingAuthorityRootBindingDto> for VotingAuthorityRootBindingV1 {
    type Error = VotingError;

    fn try_from(dto: VotingAuthorityRootBindingDto) -> Result<Self, Self::Error> {
        if dto.version != 1 {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "unsupported voting authority root binding version {}",
                    dto.version
                ),
            });
        }
        let network = parse_network_name(&dto.zcash_network)?;
        let orchard_fvk_fingerprint =
            parse_lower_hex_32("orchard_fvk_fingerprint", &dto.orchard_fvk_fingerprint)?;
        let vote_round_id = parse_lower_hex_32("vote_round_id", &dto.vote_round_id)?;
        let context = VotingAuthorityContextV1::from_fingerprint(
            network,
            dto.account_index,
            orchard_fvk_fingerprint,
            dto.vote_chain_id,
            vote_round_id,
        )?;
        let authority_source = match dto.authority_source {
            VotingAuthorityRootSourceDto::SoftwareRegisteredKey {
                application_identifier,
            } => VotingAuthorityRootSourceV1::SoftwareRegisteredKey {
                application: RegisteredKeyApplicationV1::new(application_identifier),
            },
            VotingAuthorityRootSourceDto::KeystoneMasterGeneration { generation_id } => {
                VotingAuthorityRootSourceV1::KeystoneMasterGeneration {
                    generation_id: parse_lower_hex_32("generation_id", &generation_id)?,
                }
            }
        };
        Ok(Self {
            context,
            authority_source,
            root_verification_tag: parse_lower_hex_32(
                "root_verification_tag",
                &dto.root_verification_tag,
            )?,
        })
    }
}

fn network_name(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "mainnet",
        Network::Testnet => "testnet",
        Network::Regtest => "regtest",
    }
}

fn parse_network_name(name: &str) -> Result<Network, VotingError> {
    match name {
        "mainnet" => Ok(Network::Mainnet),
        "testnet" => Ok(Network::Testnet),
        "regtest" => Ok(Network::Regtest),
        _ => Err(VotingError::InvalidInput {
            message: format!("unsupported zcash_network {name:?}"),
        }),
    }
}

fn parse_lower_hex_32(name: &str, encoded: &str) -> Result<[u8; 32], VotingError> {
    let decoded = hex::decode(encoded).map_err(|error| VotingError::InvalidInput {
        message: format!("{name} must be lowercase 32-byte hex: {error}"),
    })?;
    let bytes: [u8; 32] =
        decoded
            .try_into()
            .map_err(|decoded: Vec<u8>| VotingError::InvalidInput {
                message: format!("{name} must be 32 bytes, got {}", decoded.len()),
            })?;
    if encoded != hex::encode(bytes) {
        return Err(VotingError::InvalidInput {
            message: format!("{name} must use canonical lowercase hex"),
        });
    }
    Ok(bytes)
}

/// Secret per-round authority root.
///
/// This type is deliberately neither `Debug` nor `Serialize`. External backup
/// code must explicitly call [`Self::expose_backup_secret`].
pub struct VotingAuthorityRootV1 {
    secret: Zeroizing<[u8; VOTING_AUTHORITY_ROOT_LEN]>,
    context: VotingAuthorityContextV1,
    source: VotingAuthorityRootSourceV1,
}

impl VotingAuthorityRootV1 {
    /// Accepts only the 64-byte output of the exact registered-key request.
    pub fn from_registered_key_output(
        request: &SoftwareRegisteredKeyRequestV1,
        cryptovalue: [u8; VOTING_AUTHORITY_ROOT_LEN],
    ) -> Self {
        Self {
            secret: Zeroizing::new(cryptovalue),
            context: request.context.clone(),
            source: VotingAuthorityRootSourceV1::SoftwareRegisteredKey {
                application: request.application,
            },
        }
    }

    /// Restores a retained root only when it matches its immutable binding.
    pub fn restore(
        binding: &VotingAuthorityRootBindingV1,
        secret: [u8; VOTING_AUTHORITY_ROOT_LEN],
    ) -> Result<Self, VotingError> {
        let root = Self {
            secret: Zeroizing::new(secret),
            context: binding.context.clone(),
            source: binding.authority_source,
        };
        let actual_tag = keyed_hash32(
            root.secret_bytes(),
            ROOT_BINDING_DOMAIN,
            &binding.verification_fields(),
        );
        if actual_tag.ct_eq(&binding.root_verification_tag).into() {
            Ok(root)
        } else {
            Err(VotingError::InvalidInput {
                message: "retained voting authority root does not match binding".to_string(),
            })
        }
    }

    pub fn context(&self) -> &VotingAuthorityContextV1 {
        &self.context
    }

    pub fn source(&self) -> VotingAuthorityRootSourceV1 {
        self.source
    }

    /// Explicitly exposes root bytes for an encrypted authenticated backup.
    pub fn expose_backup_secret(&self) -> [u8; VOTING_AUTHORITY_ROOT_LEN] {
        *self.secret
    }

    /// Derives the voting-only Orchard key shared by both root sources.
    pub fn voting_hotkey(&self) -> Result<RecoverableVotingHotkeyV1, VotingError> {
        RecoverableVotingHotkeyV1::derive(self)
    }

    /// Checks this live root against an immutable public binding.
    pub fn validate_binding(
        &self,
        binding: &VotingAuthorityRootBindingV1,
    ) -> Result<(), VotingError> {
        if self.context != binding.context || self.source != binding.authority_source {
            return Err(VotingError::InvalidInput {
                message: "voting authority root context or source does not match binding"
                    .to_string(),
            });
        }
        let actual_tag = keyed_hash32(
            self.secret_bytes(),
            ROOT_BINDING_DOMAIN,
            &binding.verification_fields(),
        );
        if actual_tag.ct_eq(&binding.root_verification_tag).into() {
            Ok(())
        } else {
            Err(VotingError::InvalidInput {
                message: "voting authority root does not match binding".to_string(),
            })
        }
    }

    pub(crate) fn secret_bytes(&self) -> &[u8; VOTING_AUTHORITY_ROOT_LEN] {
        &self.secret
    }
}

/// Random host-owned Keystone master generation.
///
/// This type is deliberately neither `Debug` nor `Serialize`. Generation
/// lifecycle, encryption, rollback protection, and durable backup remain the
/// wallet's responsibility.
pub struct KeystoneMasterGenerationV1 {
    secret: Zeroizing<[u8; KEYSTONE_MASTER_SECRET_LEN]>,
    scope: VotingAuthorityScopeV1,
    generation_id: [u8; 32],
}

impl KeystoneMasterGenerationV1 {
    /// Creates a new random generation for one exact public scope.
    pub fn generate(scope: VotingAuthorityScopeV1) -> Self {
        let mut secret = Zeroizing::new([0u8; KEYSTONE_MASTER_SECRET_LEN]);
        rand::rngs::OsRng.fill_bytes(secret.as_mut());
        Self::from_zeroizing_secret(scope, secret)
    }

    /// Restores a generation only when its secret and public scope reproduce
    /// the retained generation identifier.
    pub fn restore(
        scope: VotingAuthorityScopeV1,
        secret: [u8; KEYSTONE_MASTER_SECRET_LEN],
        expected_generation_id: [u8; 32],
    ) -> Result<Self, VotingError> {
        let generation = Self::from_new_secret(scope, secret);
        if generation
            .generation_id
            .ct_eq(&expected_generation_id)
            .into()
        {
            Ok(generation)
        } else {
            Err(VotingError::InvalidInput {
                message: "Keystone voting master generation identifier mismatch".to_string(),
            })
        }
    }

    fn from_new_secret(
        scope: VotingAuthorityScopeV1,
        secret: [u8; KEYSTONE_MASTER_SECRET_LEN],
    ) -> Self {
        Self::from_zeroizing_secret(scope, Zeroizing::new(secret))
    }

    fn from_zeroizing_secret(
        scope: VotingAuthorityScopeV1,
        secret: Zeroizing<[u8; KEYSTONE_MASTER_SECRET_LEN]>,
    ) -> Self {
        let generation_id = keyed_hash32(
            secret.as_ref(),
            MASTER_GENERATION_ID_DOMAIN,
            &scope_fields(&scope),
        );
        Self {
            secret,
            scope,
            generation_id,
        }
    }

    pub fn scope(&self) -> &VotingAuthorityScopeV1 {
        &self.scope
    }

    pub fn generation_id(&self) -> [u8; 32] {
        self.generation_id
    }

    /// Explicitly exposes master bytes for an encrypted authenticated backup.
    pub fn expose_backup_secret(&self) -> [u8; KEYSTONE_MASTER_SECRET_LEN] {
        *self.secret
    }

    /// Derives one root after verifying the complete master scope matches.
    pub fn derive_round_root(
        &self,
        context: &VotingAuthorityContextV1,
    ) -> Result<VotingAuthorityRootV1, VotingError> {
        if &self.scope != context.scope() {
            return Err(VotingError::InvalidInput {
                message: "voting authority context does not match Keystone master scope"
                    .to_string(),
            });
        }
        let secret = keyed_hash64(
            self.secret.as_ref(),
            MASTER_TO_ROOT_DOMAIN,
            &[context.canonical_transcript()],
        );
        Ok(VotingAuthorityRootV1 {
            secret,
            context: context.clone(),
            source: VotingAuthorityRootSourceV1::KeystoneMasterGeneration {
                generation_id: self.generation_id,
            },
        })
    }

    /// Restores a retained round root after checking both the master-derived
    /// bytes and the binding's constant-time root verification tag.
    pub fn restore_retained_round_root(
        &self,
        binding: &VotingAuthorityRootBindingV1,
        retained_root_secret: [u8; VOTING_AUTHORITY_ROOT_LEN],
    ) -> Result<VotingAuthorityRootV1, VotingError> {
        if binding.context.scope() != &self.scope
            || binding.authority_source
                != (VotingAuthorityRootSourceV1::KeystoneMasterGeneration {
                    generation_id: self.generation_id,
                })
        {
            return Err(VotingError::InvalidInput {
                message: "authority root binding does not match Keystone master generation"
                    .to_string(),
            });
        }
        let derived = self.derive_round_root(&binding.context)?;
        if !bool::from(
            derived
                .secret_bytes()
                .ct_eq(retained_root_secret.as_slice()),
        ) {
            return Err(VotingError::InvalidInput {
                message: "retained voting authority root does not match Keystone generation"
                    .to_string(),
            });
        }
        VotingAuthorityRootV1::restore(binding, retained_root_secret)
    }
}

/// Voting-only Orchard hotkey deterministically expanded from a version 1 root.
///
/// This is distinct from the legacy random [`crate::VotingHotkey`]. Its secret
/// is an already-derived Orchard spending key and is never reinterpreted as a
/// UnifiedSpendingKey seed.
pub struct RecoverableVotingHotkeyV1 {
    spending_key: Zeroizing<[u8; 32]>,
    raw_orchard_address: [u8; 43],
    context: VotingAuthorityContextV1,
}

impl RecoverableVotingHotkeyV1 {
    fn derive(root: &VotingAuthorityRootV1) -> Result<Self, VotingError> {
        for counter in 0..=u32::MAX {
            let counter_bytes = counter.to_le_bytes();
            let mut fields = vec![root.context().canonical_transcript()];
            fields.push(counter_bytes.to_vec());
            let wide = keyed_hash64(root.secret_bytes(), ROOT_TO_HOTKEY_DOMAIN, &fields);
            let mut candidate = Zeroizing::new([0u8; 32]);
            candidate.copy_from_slice(&wide[..32]);
            if let Some(spending_key) =
                Option::<SpendingKey>::from(SpendingKey::from_bytes(*candidate))
            {
                let fvk = FullViewingKey::from(&spending_key);
                let address = fvk.address_at(
                    u64::from(crate::hotkey::VOTING_HOTKEY_ADDRESS_INDEX),
                    Scope::External,
                );
                return Ok(Self {
                    spending_key: candidate,
                    raw_orchard_address: address.to_raw_address_bytes(),
                    context: root.context.clone(),
                });
            }
        }
        Err(VotingError::Internal {
            message: "failed to expand voting authority root into an Orchard key".to_string(),
        })
    }

    pub fn context(&self) -> &VotingAuthorityContextV1 {
        &self.context
    }

    pub fn network(&self) -> Network {
        self.context.network()
    }

    pub fn raw_orchard_address(&self) -> &[u8; 43] {
        &self.raw_orchard_address
    }

    pub fn delegation_target(&self) -> VotingHotkeyTarget {
        VotingHotkeyTarget::from_raw_orchard_address(&self.raw_orchard_address, self.network())
            .expect("recoverable hotkey stores a validated Orchard address")
    }

    /// Returns the public target already bound to this authority's chain and round.
    pub fn round_bound_delegation_target(&self) -> RoundBoundVotingHotkeyTarget {
        RoundBoundVotingHotkeyTarget::from_validated_parts(
            self.delegation_target(),
            self.context.vote_chain_id().to_string(),
            *self.context.vote_round_id(),
        )
    }

    pub(crate) fn orchard_spending_key(&self) -> SpendingKey {
        Option::<SpendingKey>::from(SpendingKey::from_bytes(*self.spending_key))
            .expect("recoverable hotkey stores a validated Orchard spending key")
    }
}

fn network_tag(network: Network) -> [u8; 1] {
    [match network {
        Network::Mainnet => 0,
        Network::Testnet => 1,
        Network::Regtest => 2,
    }]
}

fn scope_fields(scope: &VotingAuthorityScopeV1) -> Vec<Vec<u8>> {
    vec![
        network_tag(scope.network).to_vec(),
        scope.account_index.to_le_bytes().to_vec(),
        scope.orchard_fvk_fingerprint.to_vec(),
        scope.vote_chain_id.as_bytes().to_vec(),
    ]
}

fn context_fields(context: &VotingAuthorityContextV1) -> Vec<Vec<u8>> {
    let mut fields = scope_fields(&context.scope);
    fields.push(context.vote_round_id.to_vec());
    fields
}

fn canonical_transcript(domain: &[u8], fields: &[Vec<u8>]) -> Vec<u8> {
    let mut transcript = Vec::new();
    append_field(&mut transcript, domain);
    for field in fields {
        append_field(&mut transcript, field);
    }
    transcript
}

fn append_field(output: &mut Vec<u8>, field: &[u8]) {
    let len = u32::try_from(field.len()).expect("authority KDF fields fit u32");
    output.extend_from_slice(&len.to_le_bytes());
    output.extend_from_slice(field);
}

pub(super) fn keyed_hash64(key: &[u8], domain: &[u8], fields: &[Vec<u8>]) -> Zeroizing<[u8; 64]> {
    let transcript = canonical_transcript(domain, fields);
    let hash = blake2b_simd::Params::new()
        .hash_length(64)
        .key(key)
        .hash(&transcript);
    Zeroizing::new(*hash.as_array())
}

fn keyed_hash32(key: &[u8], domain: &[u8], fields: &[Vec<u8>]) -> [u8; 32] {
    let transcript = canonical_transcript(domain, fields);
    let hash = blake2b_simd::Params::new()
        .hash_length(32)
        .key(key)
        .hash(&transcript);
    let mut output = [0u8; 32];
    output.copy_from_slice(hash.as_bytes());
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct AuthorityVector {
        format: String,
        context: ContextVector,
        registered_request: RegisteredRequestVector,
        keystone: KeystoneVector,
        root_binding: RootBindingVector,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ContextVector {
        network: String,
        account_index: u32,
        orchard_spending_key: String,
        orchard_fvk: String,
        orchard_fvk_fingerprint: String,
        vote_chain_id: String,
        vote_round_id: String,
        canonical_transcript: String,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RegisteredRequestVector {
        context_string: String,
        application_identifier: u16,
        application_identifier_note: String,
        synthetic_seed: String,
        cryptovalue: String,
        hotkey_address: String,
        path: Vec<RegisteredPathVector>,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RegisteredPathVector {
        index: u32,
        hardened: bool,
        tag: String,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct KeystoneVector {
        master_secret: String,
        generation_id: String,
        round_root: String,
        hotkey_address: String,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RootBindingVector {
        root_verification_tag: String,
        canonical_json: String,
    }

    fn shared_vector() -> AuthorityVector {
        serde_json::from_str(include_str!(
            "../../test-vectors/recoverable_authority_v1.json"
        ))
        .unwrap()
    }

    fn decode_hex_array<const N: usize>(encoded: &str) -> [u8; N] {
        hex::decode(encoded)
            .unwrap()
            .try_into()
            .unwrap_or_else(|bytes: Vec<u8>| panic!("expected {N} bytes, got {}", bytes.len()))
    }

    fn vector_context(vector: &AuthorityVector) -> VotingAuthorityContextV1 {
        VotingAuthorityContextV1::from_orchard_fvk(
            match vector.context.network.as_str() {
                "testnet" => Network::Testnet,
                other => panic!("unsupported test vector network {other}"),
            },
            vector.context.account_index,
            &decode_hex_array::<96>(&vector.context.orchard_fvk),
            vector.context.vote_chain_id.clone(),
            decode_hex_array(&vector.context.vote_round_id),
        )
        .unwrap()
    }

    #[test]
    fn shared_json_vector_matches_all_authority_derivations() {
        let vector = shared_vector();
        assert_eq!(vector.format, "zcash_voting_recoverable_authority_v1");

        let spending_key = Option::<SpendingKey>::from(SpendingKey::from_bytes(decode_hex_array(
            &vector.context.orchard_spending_key,
        )))
        .expect("test vector spending key is valid");
        assert_eq!(
            hex::encode(FullViewingKey::from(&spending_key).to_bytes()),
            vector.context.orchard_fvk
        );
        let context = vector_context(&vector);
        assert_eq!(
            hex::encode(context.orchard_fvk_fingerprint()),
            vector.context.orchard_fvk_fingerprint
        );
        assert_eq!(
            hex::encode(context.canonical_transcript()),
            vector.context.canonical_transcript
        );

        let request = SoftwareRegisteredKeyRequestV1::new(
            RegisteredKeyApplicationV1::new(vector.registered_request.application_identifier),
            context.clone(),
        );
        assert_eq!(
            vector.registered_request.application_identifier_note,
            "test-only unallocated example; replace with an allocated ZIP number before deployment"
        );
        assert_eq!(
            request.context_string(),
            vector.registered_request.context_string.as_bytes()
        );
        assert_eq!(vector.registered_request.path.len(), 1);
        let expected_path = &vector.registered_request.path[0];
        let actual_path = request.path_element();
        assert_eq!(request.child_index(), expected_path.index);
        assert_eq!(
            actual_path.child_index().index(),
            expected_path.index | (1 << 31)
        );
        assert!(expected_path.hardened);
        assert_eq!(hex::encode(actual_path.tag()), expected_path.tag);

        let synthetic_seed = decode_hex_array::<32>(&vector.registered_request.synthetic_seed);
        let software_output = zip32::registered::cryptovalue_from_subpath(
            request.context_string(),
            &synthetic_seed,
            request.application_identifier(),
            &[request.path_element()],
        )
        .unwrap();
        assert_eq!(
            hex::encode(software_output),
            vector.registered_request.cryptovalue
        );
        let software_root =
            VotingAuthorityRootV1::from_registered_key_output(&request, software_output);
        assert_eq!(
            hex::encode(software_root.voting_hotkey().unwrap().raw_orchard_address()),
            vector.registered_request.hotkey_address
        );

        let master = KeystoneMasterGenerationV1::from_new_secret(
            context.scope().clone(),
            decode_hex_array(&vector.keystone.master_secret),
        );
        assert_eq!(
            hex::encode(master.generation_id()),
            vector.keystone.generation_id
        );
        let root = master.derive_round_root(&context).unwrap();
        assert_eq!(
            hex::encode(root.expose_backup_secret()),
            vector.keystone.round_root
        );
        assert_eq!(
            hex::encode(root.voting_hotkey().unwrap().raw_orchard_address()),
            vector.keystone.hotkey_address
        );

        let vector_binding = VotingAuthorityRootBindingV1::bind(&root);
        assert_eq!(
            hex::encode(vector_binding.root_verification_tag()),
            vector.root_binding.root_verification_tag
        );
        assert_eq!(
            vector_binding.to_json().unwrap(),
            vector.root_binding.canonical_json
        );
        assert_eq!(
            VotingAuthorityRootBindingV1::from_json(&vector.root_binding.canonical_json).unwrap(),
            vector_binding
        );

        software_root
            .validate_binding(&VotingAuthorityRootBindingV1::bind(&software_root))
            .unwrap();
    }

    fn context() -> VotingAuthorityContextV1 {
        VotingAuthorityContextV1::from_fingerprint(
            Network::Testnet,
            7,
            [0x22; 32],
            "vote-chain-test",
            [0x01; 32],
        )
        .unwrap()
    }

    #[test]
    fn registered_key_request_is_seed_free_and_nonempty() {
        let request =
            SoftwareRegisteredKeyRequestV1::new(RegisteredKeyApplicationV1::new(0xA11C), context());
        assert_eq!(
            request.context_string(),
            b"valargroup.zcash_voting.authority.v1"
        );
        assert_eq!(request.application_identifier(), 0xA11C);
        assert_eq!(request.child_index(), 7);
        assert!(!request.tag().is_empty());
    }

    #[test]
    fn master_generation_verifies_id_and_separates_context() {
        let context = context();
        let master = KeystoneMasterGenerationV1::from_new_secret(
            context.scope().clone(),
            [0x33; KEYSTONE_MASTER_SECRET_LEN],
        );
        let restored = KeystoneMasterGenerationV1::restore(
            context.scope().clone(),
            master.expose_backup_secret(),
            master.generation_id(),
        )
        .unwrap();
        assert_eq!(restored.generation_id(), master.generation_id());

        let mut wrong_id = master.generation_id();
        wrong_id[0] ^= 1;
        assert!(KeystoneMasterGenerationV1::restore(
            context.scope().clone(),
            master.expose_backup_secret(),
            wrong_id,
        )
        .is_err());

        let first = master.derive_round_root(&context).unwrap();
        let other_context = VotingAuthorityContextV1::from_fingerprint(
            Network::Testnet,
            7,
            [0x22; 32],
            "vote-chain-test",
            [0x02; 32],
        )
        .unwrap();
        let second = master.derive_round_root(&other_context).unwrap();
        assert_ne!(first.expose_backup_secret(), second.expose_backup_secret());
    }

    #[test]
    fn every_public_context_field_separates_registered_and_keystone_derivation() {
        let base = context();
        let alternatives = [
            VotingAuthorityContextV1::from_fingerprint(
                Network::Regtest,
                7,
                [0x22; 32],
                "vote-chain-test",
                [0x01; 32],
            )
            .unwrap(),
            VotingAuthorityContextV1::from_fingerprint(
                Network::Testnet,
                8,
                [0x22; 32],
                "vote-chain-test",
                [0x01; 32],
            )
            .unwrap(),
            VotingAuthorityContextV1::from_fingerprint(
                Network::Testnet,
                7,
                [0x23; 32],
                "vote-chain-test",
                [0x01; 32],
            )
            .unwrap(),
            VotingAuthorityContextV1::from_fingerprint(
                Network::Testnet,
                7,
                [0x22; 32],
                "vote-chain-other",
                [0x01; 32],
            )
            .unwrap(),
            VotingAuthorityContextV1::from_fingerprint(
                Network::Testnet,
                7,
                [0x22; 32],
                "vote-chain-test",
                [0x02; 32],
            )
            .unwrap(),
        ];
        let application = RegisteredKeyApplicationV1::new(0xA11C);
        let base_request = SoftwareRegisteredKeyRequestV1::new(application, base.clone());
        let base_master = KeystoneMasterGenerationV1::from_new_secret(
            base.scope().clone(),
            [0x33; KEYSTONE_MASTER_SECRET_LEN],
        );

        for alternative in alternatives {
            let request = SoftwareRegisteredKeyRequestV1::new(application, alternative.clone());
            assert_ne!(request.tag(), base_request.tag());
            assert_ne!(
                alternative.canonical_transcript(),
                base.canonical_transcript()
            );
            if alternative.scope() == base.scope() {
                let root = base_master.derive_round_root(&base).unwrap();
                let other_root = base_master.derive_round_root(&alternative).unwrap();
                assert_ne!(
                    root.expose_backup_secret(),
                    other_root.expose_backup_secret()
                );
            } else {
                let master = KeystoneMasterGenerationV1::from_new_secret(
                    alternative.scope().clone(),
                    [0x33; KEYSTONE_MASTER_SECRET_LEN],
                );
                assert_ne!(master.generation_id(), base_master.generation_id());
                assert!(base_master.derive_round_root(&alternative).is_err());
            }
        }
    }

    #[test]
    fn source_types_remain_distinct() {
        let request =
            SoftwareRegisteredKeyRequestV1::new(RegisteredKeyApplicationV1::new(0xA11C), context());
        let software = VotingAuthorityRootV1::from_registered_key_output(&request, [0x55; 64]);
        let master =
            KeystoneMasterGenerationV1::from_new_secret(context().scope().clone(), [0x44; 64]);
        let keystone = master.derive_round_root(&context()).unwrap();

        assert!(matches!(
            software.source(),
            VotingAuthorityRootSourceV1::SoftwareRegisteredKey { .. }
        ));
        assert!(matches!(
            keystone.source(),
            VotingAuthorityRootSourceV1::KeystoneMasterGeneration { .. }
        ));
        assert_ne!(
            software.expose_backup_secret(),
            keystone.expose_backup_secret()
        );
    }

    #[test]
    fn root_binding_json_is_strict_and_restores_matching_root() {
        let context = context();
        let master = KeystoneMasterGenerationV1::from_new_secret(
            context.scope().clone(),
            [0x33; KEYSTONE_MASTER_SECRET_LEN],
        );
        let root = master.derive_round_root(&context).unwrap();
        let binding = VotingAuthorityRootBindingV1::bind(&root);

        let json = binding.to_json().unwrap();
        let decoded = VotingAuthorityRootBindingV1::from_json(&json).unwrap();
        assert_eq!(decoded, binding);
        root.validate_binding(&decoded).unwrap();
        VotingAuthorityRootV1::restore(&decoded, root.expose_backup_secret()).unwrap();
        master
            .restore_retained_round_root(&decoded, root.expose_backup_secret())
            .unwrap();

        let mut wrong_root = root.expose_backup_secret();
        wrong_root[0] ^= 1;
        assert!(VotingAuthorityRootV1::restore(&decoded, wrong_root).is_err());
        assert!(master
            .restore_retained_round_root(&decoded, wrong_root)
            .is_err());

        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["unexpected"] = serde_json::json!(true);
        assert!(VotingAuthorityRootBindingV1::from_json(&value.to_string()).is_err());

        let uppercase = json.replace(
            &hex::encode(master.generation_id()),
            &hex::encode_upper(master.generation_id()),
        );
        assert!(VotingAuthorityRootBindingV1::from_json(&uppercase).is_err());
    }
}
