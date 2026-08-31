//! Canonical payloads signed by dynamic-config round attestations.

use crate::config::PirLayout;
use crate::types::{
    validate_vote_chain_id, Network, VotingError, MAX_PROPOSAL_ID, MIN_PROPOSAL_ID,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Dynamic-config authentication version using [`RoundAuthPayloadV2`].
pub const ROUND_AUTH_VERSION_V2: u32 = 2;

/// Dynamic-config authentication version using [`RoundAuthPayloadV3`].
pub const ROUND_AUTH_VERSION_V3: u32 = 3;

/// Authority construction selected by round-auth version 3.
pub const RECOVERABLE_AUTHORITY_SCHEME_V1: &str = "recoverable-authority-v1";

/// Snapshot bundle policy selected by round-auth version 3.
pub const RECOVERABLE_BUNDLE_POLICY_V1: &str = "recoverable-v1";

const ROUND_AUTH_DOMAIN_TAG_V2: [u8; 33] = *b"zcash-shielded-vote:round-auth:v2";
const ROUND_AUTH_DOMAIN_TAG_V3: [u8; 33] = *b"zcash-shielded-vote:round-auth:v3";
const ROUND_AUTH_V3_FIELD_COUNT: u8 = 13;

/// Typed round-auth v2 signing payload.
///
/// Its fixed-width encoding is
/// `domain || round_id || ea_pk || pir_depth || tier0_layers || tier1_layers || poly_len`,
/// with each `u32` encoded little-endian. This binds each attestation to its
/// round and the full advertised [`PirLayout`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundAuthPayloadV2 {
    domain: [u8; 33],
    round_id: [u8; 32],
    ea_pk: [u8; 32],
    pir_depth: u32,
    tier0_layers: u32,
    tier1_layers: u32,
    poly_len: u32,
}

impl RoundAuthPayloadV2 {
    /// Constructs the payload for a round and its advertised PIR layout.
    pub fn new(round_id: [u8; 32], ea_pk: [u8; 32], pir_layout: PirLayout) -> Self {
        Self {
            domain: ROUND_AUTH_DOMAIN_TAG_V2,
            round_id,
            ea_pk,
            pir_depth: pir_layout.pir_depth,
            tier0_layers: pir_layout.tier0_layers,
            tier1_layers: pir_layout.tier1_layers,
            poly_len: pir_layout.poly_len,
        }
    }

    /// Returns the canonical fixed-width bytes to sign or verify.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(33 + 32 + 32 + 16);
        bytes.extend_from_slice(&self.domain);
        bytes.extend_from_slice(&self.round_id);
        bytes.extend_from_slice(&self.ea_pk);
        bytes.extend_from_slice(&self.pir_depth.to_le_bytes());
        bytes.extend_from_slice(&self.tier0_layers.to_le_bytes());
        bytes.extend_from_slice(&self.tier1_layers.to_le_bytes());
        bytes.extend_from_slice(&self.poly_len.to_le_bytes());
        bytes
    }
}

/// Independently selected network, chain, and snapshot context for a
/// recoverable-authority round.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedRoundAuthContextV3")]
pub struct RoundAuthContextV3 {
    #[serde(with = "network_serde")]
    network: Network,
    vote_chain_id: String,
    snapshot_height: u64,
    #[serde(with = "array32_base64_serde")]
    snapshot_block_hash: [u8; 32],
    proposal_count: u32,
}

#[derive(Deserialize)]
struct UncheckedRoundAuthContextV3 {
    #[serde(with = "network_serde")]
    network: Network,
    vote_chain_id: String,
    snapshot_height: u64,
    #[serde(with = "array32_base64_serde")]
    snapshot_block_hash: [u8; 32],
    proposal_count: u32,
}

impl TryFrom<UncheckedRoundAuthContextV3> for RoundAuthContextV3 {
    type Error = VotingError;

    fn try_from(value: UncheckedRoundAuthContextV3) -> Result<Self, Self::Error> {
        Self::new(
            value.network,
            value.vote_chain_id,
            value.snapshot_height,
            value.snapshot_block_hash,
            value.proposal_count,
        )
    }
}

impl RoundAuthContextV3 {
    /// Validates and constructs the context signed by a version 3 round entry.
    pub fn new(
        network: Network,
        vote_chain_id: impl Into<String>,
        snapshot_height: u64,
        snapshot_block_hash: [u8; 32],
        proposal_count: u32,
    ) -> Result<Self, VotingError> {
        let vote_chain_id = vote_chain_id.into();
        validate_vote_chain_id(&vote_chain_id)?;
        if !(MIN_PROPOSAL_ID..=MAX_PROPOSAL_ID).contains(&proposal_count) {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "proposal_count must be {MIN_PROPOSAL_ID}..={MAX_PROPOSAL_ID}, got {proposal_count}"
                ),
            });
        }
        Ok(Self {
            network,
            vote_chain_id,
            snapshot_height,
            snapshot_block_hash,
            proposal_count,
        })
    }

    /// Returns the Zcash network bound by the round attestation.
    pub fn network(&self) -> Network {
        self.network
    }

    /// Returns the exact vote-chain identifier bound by the attestation.
    pub fn vote_chain_id(&self) -> &str {
        &self.vote_chain_id
    }

    /// Returns the authenticated Zcash snapshot height.
    pub fn snapshot_height(&self) -> u64 {
        self.snapshot_height
    }

    /// Returns the authenticated block hash at the snapshot height.
    pub fn snapshot_block_hash(&self) -> &[u8; 32] {
        &self.snapshot_block_hash
    }

    /// Returns the authenticated number of sequential proposal IDs in the round.
    pub fn proposal_count(&self) -> u32 {
        self.proposal_count
    }
}

/// Typed round-auth v3 signing payload.
///
/// The encoding starts with the version-specific domain followed by a field
/// count and thirteen ordered fields. Each field is encoded as
/// `tag || byte_length_le_u32 || value`. The tags and lengths make the
/// encoding prefix-free while retaining fixed canonical encodings for numeric
/// values. It binds the recoverable-authority scheme and bundle policy, the
/// independently selected network and vote chain, the exact Zcash snapshot,
/// and every value already covered by round-auth v2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundAuthPayloadV3 {
    round_id: [u8; 32],
    ea_pk: [u8; 32],
    pir_layout: PirLayout,
    context: RoundAuthContextV3,
}

/// Canonical identity of one fully authenticated v3 round payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RoundAuthDigestV3([u8; 32]);

impl RoundAuthDigestV3 {
    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the exact SHA-256 digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Returns the canonical lowercase hexadecimal digest.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl RoundAuthPayloadV3 {
    /// Constructs a canonical recoverable-authority round payload.
    pub fn new(
        round_id: [u8; 32],
        ea_pk: [u8; 32],
        pir_layout: PirLayout,
        context: RoundAuthContextV3,
    ) -> Self {
        Self {
            round_id,
            ea_pk,
            pir_layout,
            context,
        }
    }

    /// Returns the exact round identifier covered by this payload.
    pub fn round_id(&self) -> &[u8; 32] {
        &self.round_id
    }

    /// Returns the election-authority key covered by this payload.
    pub fn ea_pk(&self) -> &[u8; 32] {
        &self.ea_pk
    }

    /// Returns the PIR layout covered by this payload.
    pub fn pir_layout(&self) -> PirLayout {
        self.pir_layout
    }

    /// Returns the authenticated version 3 context.
    pub fn context(&self) -> &RoundAuthContextV3 {
        &self.context
    }

    /// Returns the canonical prefix-free bytes to sign or verify.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(33 + 1 + 13 * 5 + 24 + 14 + 7 + 32 + 32 + 8 + 32 + 20);
        bytes.extend_from_slice(&ROUND_AUTH_DOMAIN_TAG_V3);
        bytes.push(ROUND_AUTH_V3_FIELD_COUNT);
        append_v3_field(&mut bytes, 1, RECOVERABLE_AUTHORITY_SCHEME_V1.as_bytes());
        append_v3_field(&mut bytes, 2, RECOVERABLE_BUNDLE_POLICY_V1.as_bytes());
        append_v3_field(
            &mut bytes,
            3,
            network_name(self.context.network()).as_bytes(),
        );
        append_v3_field(&mut bytes, 4, self.context.vote_chain_id().as_bytes());
        append_v3_field(&mut bytes, 5, &self.round_id);
        append_v3_field(&mut bytes, 6, &self.ea_pk);
        append_v3_field(&mut bytes, 7, &self.context.snapshot_height().to_le_bytes());
        append_v3_field(&mut bytes, 8, self.context.snapshot_block_hash());
        append_v3_field(&mut bytes, 9, &self.context.proposal_count().to_le_bytes());
        append_v3_field(&mut bytes, 10, &self.pir_layout.pir_depth.to_le_bytes());
        append_v3_field(&mut bytes, 11, &self.pir_layout.tier0_layers.to_le_bytes());
        append_v3_field(&mut bytes, 12, &self.pir_layout.tier1_layers.to_le_bytes());
        append_v3_field(&mut bytes, 13, &self.pir_layout.poly_len.to_le_bytes());
        bytes
    }

    /// Hashes the complete canonical signed payload.
    pub fn digest(&self) -> RoundAuthDigestV3 {
        RoundAuthDigestV3(Sha256::digest(self.to_bytes()).into())
    }
}

fn append_v3_field(bytes: &mut Vec<u8>, tag: u8, value: &[u8]) {
    let value_len = u32::try_from(value.len()).expect("round-auth field lengths fit u32");
    bytes.push(tag);
    bytes.extend_from_slice(&value_len.to_le_bytes());
    bytes.extend_from_slice(value);
}

fn network_name(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "mainnet",
        Network::Testnet => "testnet",
        Network::Regtest => "regtest",
    }
}

mod network_serde {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(network: &Network, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(network_name(*network))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Network, D::Error>
    where
        D: Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "mainnet" => Ok(Network::Mainnet),
            "testnet" => Ok(Network::Testnet),
            "regtest" => Ok(Network::Regtest),
            value => Err(serde::de::Error::custom(format!(
                "unsupported Zcash network {value}"
            ))),
        }
    }
}

mod array32_base64_serde {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&BASE64.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 32], D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        let bytes = BASE64.decode(encoded).map_err(serde::de::Error::custom)?;
        <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
            serde::de::Error::custom("snapshot_block_hash must decode to exactly 32 bytes")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_matches_round_auth_v2_wire_format() {
        let round_id = [1u8; 32];
        let ea_pk = [2u8; 32];
        let layout = PirLayout {
            pir_depth: 19,
            tier0_layers: 12,
            tier1_layers: 7,
            poly_len: 4096,
        };

        let mut expected = ROUND_AUTH_DOMAIN_TAG_V2.to_vec();
        expected.extend_from_slice(&round_id);
        expected.extend_from_slice(&ea_pk);
        expected.extend_from_slice(&19u32.to_le_bytes());
        expected.extend_from_slice(&12u32.to_le_bytes());
        expected.extend_from_slice(&7u32.to_le_bytes());
        expected.extend_from_slice(&4096u32.to_le_bytes());

        assert_eq!(
            RoundAuthPayloadV2::new(round_id, ea_pk, layout).to_bytes(),
            expected
        );
    }

    #[test]
    fn encoding_matches_prefix_free_round_auth_v3_wire_format() {
        let round_id = [1u8; 32];
        let ea_pk = [2u8; 32];
        let layout = PirLayout {
            pir_depth: 19,
            tier0_layers: 12,
            tier1_layers: 7,
            poly_len: 4096,
        };
        let context =
            RoundAuthContextV3::new(Network::Testnet, "vote-chain-1", 1_234_567, [3u8; 32], 3)
                .unwrap();

        let encoded = RoundAuthPayloadV3::new(round_id, ea_pk, layout, context).to_bytes();
        assert_eq!(
            hex::encode(&encoded),
            concat!(
                "7a636173682d736869656c6465642d766f74653a726f756e642d617574683a76330d",
                "01180000007265636f76657261626c652d617574686f726974792d7631",
                "020e0000007265636f76657261626c652d7631",
                "0307000000746573746e6574",
                "040c000000766f74652d636861696e2d31",
                "05200000000101010101010101010101010101010101010101010101010101010101010101",
                "06200000000202020202020202020202020202020202020202020202020202020202020202",
                "070800000087d6120000000000",
                "08200000000303030303030303030303030303030303030303030303030303030303030303",
                "090400000003000000",
                "0a0400000013000000",
                "0b040000000c000000",
                "0c0400000007000000",
                "0d0400000000100000"
            )
        );
        assert_eq!(
            hex::encode(Sha256::digest(&encoded)),
            "f155b3aa1a949dd1fa0e7ede5c1f49e15f096cd7a6f83491dd933fa449019d0a"
        );
        assert_eq!(
            RoundAuthPayloadV3::new(
                round_id,
                ea_pk,
                layout,
                RoundAuthContextV3::new(Network::Testnet, "vote-chain-1", 1_234_567, [3u8; 32], 3,)
                    .unwrap(),
            )
            .digest()
            .to_hex(),
            "f155b3aa1a949dd1fa0e7ede5c1f49e15f096cd7a6f83491dd933fa449019d0a"
        );
    }

    #[test]
    fn round_auth_v3_context_rejects_noncanonical_vote_chain_ids() {
        for invalid in ["", "chain id", "chain\n"] {
            assert!(RoundAuthContextV3::new(Network::Mainnet, invalid, 1, [0; 32], 1).is_err());
        }
        assert!(RoundAuthContextV3::new(Network::Mainnet, "x".repeat(129), 1, [0; 32], 1).is_err());
        assert!(RoundAuthContextV3::new(Network::Mainnet, "chain", 1, [0; 32], 0).is_err());
        assert!(RoundAuthContextV3::new(Network::Mainnet, "chain", 1, [0; 32], 16).is_err());

        let invalid_json = serde_json::json!({
            "network": "mainnet",
            "vote_chain_id": "chain id",
            "snapshot_height": 1,
            "snapshot_block_hash": BASE64.encode([0u8; 32]),
            "proposal_count": 1,
        });
        assert!(serde_json::from_value::<RoundAuthContextV3>(invalid_json).is_err());
    }

    #[test]
    fn round_auth_v3_context_serde_is_canonical_and_validated() {
        let context =
            RoundAuthContextV3::new(Network::Regtest, "vote-chain-test", 42, [0x33; 32], 3)
                .unwrap();
        let encoded = serde_json::to_value(&context).unwrap();
        assert_eq!(encoded["network"], "regtest");
        assert_eq!(encoded["snapshot_block_hash"], BASE64.encode([0x33; 32]));
        assert_eq!(encoded["proposal_count"], 3);
        assert_eq!(
            serde_json::from_value::<RoundAuthContextV3>(encoded).unwrap(),
            context
        );
    }

    #[test]
    fn round_auth_v3_length_prefixes_separate_adjacent_values() {
        let layout = PirLayout {
            pir_depth: 19,
            tier0_layers: 12,
            tier1_layers: 7,
            poly_len: 4096,
        };
        let first = RoundAuthPayloadV3::new(
            [1; 32],
            [2; 32],
            layout,
            RoundAuthContextV3::new(Network::Regtest, "ab", 12, [3; 32], 3).unwrap(),
        )
        .to_bytes();
        let second = RoundAuthPayloadV3::new(
            [1; 32],
            [2; 32],
            layout,
            RoundAuthContextV3::new(Network::Regtest, "a", 12, [3; 32], 3).unwrap(),
        )
        .to_bytes();

        assert_ne!(first, second);
    }
}
