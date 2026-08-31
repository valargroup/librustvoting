use crate::{
    config::{PirLayout, ResolvedVotingConfig},
    types::{
        validate_round_params, validate_vote_chain_id, Network, VotingError, MAX_PROPOSAL_ID,
        MIN_PROPOSAL_ID,
    },
    wire::VotingRoundParams,
};

use super::VotingAuthorityContextV1;

/// Vote-chain fields required by recoverable voting.
///
/// The host must populate this value from the selected vote chain's round
/// query. `zcash_voting` validates its round identity and election key against
/// the existing signed dynamic config before returning a usable round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainVotingRoundV1 {
    round_id: [u8; 32],
    round_params: VotingRoundParams,
    snapshot_block_hash: [u8; 32],
    proposal_count: u32,
}

impl ChainVotingRoundV1 {
    pub fn new(
        round_params: VotingRoundParams,
        snapshot_block_hash: [u8; 32],
        proposal_count: u32,
    ) -> Result<Self, VotingError> {
        validate_round_params(&round_params)?;
        if !(MIN_PROPOSAL_ID..=MAX_PROPOSAL_ID).contains(&proposal_count) {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "vote-chain proposal count must be {MIN_PROPOSAL_ID}..={MAX_PROPOSAL_ID}, got {proposal_count}"
                ),
            });
        }
        let round_id = hex::decode(&round_params.vote_round_id)
            .expect("validated round ID is hexadecimal")
            .try_into()
            .expect("validated round ID is 32 bytes");
        Ok(Self {
            round_id,
            round_params,
            snapshot_block_hash,
            proposal_count,
        })
    }

    pub fn round_params(&self) -> &VotingRoundParams {
        &self.round_params
    }

    pub fn snapshot_block_hash(&self) -> &[u8; 32] {
        &self.snapshot_block_hash
    }

    pub fn proposal_count(&self) -> u32 {
        self.proposal_count
    }
}

/// A chain round joined to the existing signed dynamic-config entry.
///
/// Private fields ensure downstream recoverable APIs receive round parameters,
/// PIR metadata, network, and chain identity that were validated together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedRecoverableVotingRoundV1 {
    network: Network,
    vote_chain_id: String,
    chain_round: ChainVotingRoundV1,
    pir_layout: PirLayout,
    configured_pir_endpoints: Vec<String>,
}

impl ValidatedRecoverableVotingRoundV1 {
    pub fn network(&self) -> Network {
        self.network
    }

    pub fn vote_chain_id(&self) -> &str {
        &self.vote_chain_id
    }

    pub fn round_params(&self) -> &VotingRoundParams {
        self.chain_round.round_params()
    }

    pub fn round_id(&self) -> &str {
        &self.chain_round.round_params.vote_round_id
    }

    pub fn round_id_bytes(&self) -> [u8; 32] {
        self.chain_round.round_id
    }

    pub fn snapshot_height(&self) -> u64 {
        self.chain_round.round_params.snapshot_height
    }

    pub fn snapshot_block_hash(&self) -> &[u8; 32] {
        self.chain_round.snapshot_block_hash()
    }

    pub fn proposal_count(&self) -> u32 {
        self.chain_round.proposal_count()
    }

    pub fn pir_layout(&self) -> PirLayout {
        self.pir_layout
    }

    pub(crate) fn permits_pir_endpoint(&self, endpoint: &str) -> bool {
        let endpoint = crate::pir::normalize_endpoint_url(endpoint);
        self.configured_pir_endpoints
            .iter()
            .any(|configured| configured == &endpoint)
    }

    pub(crate) fn validate_authority_context(
        &self,
        context: &VotingAuthorityContextV1,
    ) -> Result<(), VotingError> {
        if context.network() != self.network
            || context.vote_chain_id() != self.vote_chain_id
            || context.vote_round_id() != &self.round_id_bytes()
        {
            return Err(VotingError::InvalidInput {
                message: "recoverable voting round does not match authority context".to_string(),
            });
        }
        Ok(())
    }
}

/// Joins vote-chain round data to the round ID, election key, and PIR layout
/// already authenticated by dynamic-config version 2.
pub fn validate_recoverable_voting_round_v1(
    config: &ResolvedVotingConfig,
    network: Network,
    vote_chain_id: impl Into<String>,
    chain_round: ChainVotingRoundV1,
) -> Result<ValidatedRecoverableVotingRoundV1, VotingError> {
    let vote_chain_id = vote_chain_id.into();
    validate_vote_chain_id(&vote_chain_id)?;

    let round_id = &chain_round.round_params.vote_round_id;
    let authenticated_round = config
        .authenticated_rounds
        .iter()
        .find(|round| &round.round_id == round_id)
        .ok_or_else(|| VotingError::InvalidInput {
            message: format!("vote-chain round {round_id} is not authenticated by dynamic config"),
        })?;
    if authenticated_round.ea_pk != chain_round.round_params.ea_pk {
        return Err(VotingError::InvalidInput {
            message: format!(
                "vote-chain election key for round {round_id} does not match dynamic config"
            ),
        });
    }

    Ok(ValidatedRecoverableVotingRoundV1 {
        network,
        vote_chain_id,
        chain_round,
        pir_layout: config.pir_layout,
        configured_pir_endpoints: config
            .pir_endpoints
            .iter()
            .map(|endpoint| crate::pir::normalize_endpoint_url(&endpoint.url))
            .collect(),
    })
}

#[cfg(test)]
pub(crate) fn test_validated_recoverable_voting_round_v1(
    network: Network,
    vote_chain_id: &str,
    round_params: VotingRoundParams,
    snapshot_block_hash: [u8; 32],
    proposal_count: u32,
) -> ValidatedRecoverableVotingRoundV1 {
    use crate::config::{AuthenticatedRound, ServiceEndpoint, SupportedVersions};

    let config = ResolvedVotingConfig {
        source_fingerprint: "test-source".to_string(),
        trusted_key_fingerprint: "test-key".to_string(),
        dynamic_config_fingerprint: "test-dynamic-config".to_string(),
        vote_servers: vec![],
        pir_endpoints: vec![ServiceEndpoint {
            url: "https://pir.example.com/".to_string(),
            label: "PIR".to_string(),
        }],
        pir_layout: PirLayout {
            pir_depth: 19,
            tier0_layers: 12,
            tier1_layers: 7,
            poly_len: 4096,
        },
        supported_versions: SupportedVersions {
            pir: vec!["v0".to_string()],
            vote_protocol: "v0".to_string(),
            tally: "v0".to_string(),
            vote_server: "v1".to_string(),
        },
        authenticated_rounds: vec![AuthenticatedRound {
            round_id: round_params.vote_round_id.clone(),
            ea_pk: round_params.ea_pk.clone(),
        }],
        skipped_round_ids: vec![],
        conditions: vec![],
    };
    let chain_round =
        ChainVotingRoundV1::new(round_params, snapshot_block_hash, proposal_count).unwrap();
    validate_recoverable_voting_round_v1(&config, network, vote_chain_id, chain_round).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthenticatedRound, ServiceEndpoint};

    fn resolved_config() -> ResolvedVotingConfig {
        ResolvedVotingConfig {
            source_fingerprint: "source".to_string(),
            trusted_key_fingerprint: "key".to_string(),
            dynamic_config_fingerprint: "dynamic".to_string(),
            vote_servers: vec![],
            pir_endpoints: vec![ServiceEndpoint {
                url: "https://pir.example.com/".to_string(),
                label: "PIR".to_string(),
            }],
            pir_layout: PirLayout {
                pir_depth: 19,
                tier0_layers: 12,
                tier1_layers: 7,
                poly_len: 4096,
            },
            supported_versions: crate::config::SupportedVersions {
                pir: vec!["v0".to_string()],
                vote_protocol: "v0".to_string(),
                tally: "v0".to_string(),
                vote_server: "v1".to_string(),
            },
            authenticated_rounds: vec![AuthenticatedRound {
                round_id: "01".repeat(32),
                ea_pk: vec![0xEA; 32],
            }],
            skipped_round_ids: vec![],
            conditions: vec![],
        }
    }

    fn chain_round() -> ChainVotingRoundV1 {
        ChainVotingRoundV1::new(
            VotingRoundParams {
                vote_round_id: "01".repeat(32),
                snapshot_height: 1_234_567,
                ea_pk: vec![0xEA; 32],
                nc_root: vec![0x11; 32],
                nullifier_imt_root: vec![0x22; 32],
            },
            [0xAB; 32],
            3,
        )
        .unwrap()
    }

    #[test]
    fn existing_v2_config_binds_the_chain_round() {
        let round = validate_recoverable_voting_round_v1(
            &resolved_config(),
            Network::Testnet,
            "vote-chain-test",
            chain_round(),
        )
        .unwrap();
        assert_eq!(round.round_id(), "01".repeat(32));
        assert_eq!(round.round_id_bytes(), [1; 32]);
        assert_eq!(round.snapshot_block_hash(), &[0xAB; 32]);
        assert_eq!(round.proposal_count(), 3);
        assert!(round.permits_pir_endpoint("https://pir.example.com"));
    }

    #[test]
    fn mismatched_chain_election_key_is_rejected() {
        let mut chain_round = chain_round();
        chain_round.round_params.ea_pk = vec![0xFF; 32];
        let error = validate_recoverable_voting_round_v1(
            &resolved_config(),
            Network::Testnet,
            "vote-chain-test",
            chain_round,
        )
        .unwrap_err();
        assert!(error.to_string().contains("election key"), "{error}");
    }
}
