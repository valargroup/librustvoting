//! Exact-snapshot PIR binding for recoverable voting authority.

use std::sync::Arc;

use crate::{
    config::PirLayout,
    pir::{PirClientBlocking, Transport},
    types::VotingError,
};

use super::ValidatedRecoverableVotingRoundV1;

/// Exact metadata obtained from a PIR snapshot source that reports block hash.
///
/// The current height-only `/root` response cannot produce this value. An
/// integration must obtain all fields together from an exact metadata source;
/// this constructor only validates their shape before trusted round matching.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoverablePirSnapshotMetadataV1 {
    endpoint: String,
    snapshot_height: u64,
    snapshot_block_hash: [u8; 32],
    pir_layout: PirLayout,
}

impl RecoverablePirSnapshotMetadataV1 {
    pub fn new(
        endpoint: impl Into<String>,
        snapshot_height: u64,
        snapshot_block_hash: [u8; 32],
        pir_layout: PirLayout,
    ) -> Result<Self, VotingError> {
        let endpoint = crate::pir::normalize_endpoint_url(&endpoint.into());
        if endpoint.is_empty() {
            return Err(invalid("recoverable PIR endpoint URL must not be empty"));
        }
        crate::pir::negotiated_pir_layout(pir_layout)?;
        Ok(Self {
            endpoint,
            snapshot_height,
            snapshot_block_hash,
            pir_layout,
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn snapshot_height(&self) -> u64 {
        self.snapshot_height
    }

    pub fn snapshot_block_hash(&self) -> &[u8; 32] {
        &self.snapshot_block_hash
    }

    pub fn pir_layout(&self) -> PirLayout {
        self.pir_layout
    }
}

/// Exact PIR snapshot selected against one validated chain round.
///
/// This token is opaque and not serializable. It therefore cannot be rebuilt
/// from the legacy height-only [`crate::pir_snapshot::PirSnapshotResolution`].
#[derive(Clone, PartialEq, Eq)]
pub struct VerifiedRecoverablePirSnapshotV1 {
    round: ValidatedRecoverableVotingRoundV1,
    metadata: RecoverablePirSnapshotMetadataV1,
}

impl VerifiedRecoverablePirSnapshotV1 {
    pub fn endpoint(&self) -> &str {
        self.metadata.endpoint()
    }

    pub fn snapshot_height(&self) -> u64 {
        self.metadata.snapshot_height()
    }

    pub fn snapshot_block_hash(&self) -> &[u8; 32] {
        self.metadata.snapshot_block_hash()
    }

    pub fn pir_layout(&self) -> PirLayout {
        self.metadata.pir_layout()
    }

    fn validate_round(&self, round: &ValidatedRecoverableVotingRoundV1) -> Result<(), VotingError> {
        if &self.round != round
            || self.snapshot_height() != round.snapshot_height()
            || self.snapshot_block_hash() != round.snapshot_block_hash()
            || self.pir_layout() != round.pir_layout()
            || !round.permits_pir_endpoint(self.endpoint())
        {
            return Err(invalid(
                "recoverable PIR client does not match the verified voting round",
            ));
        }
        Ok(())
    }
}

/// Selects exact metadata that matches one validated chain round in every field.
///
/// `match_index` lets callers inject deterministic or random selection without
/// turning endpoint probing into part of this API.
pub fn select_recoverable_pir_snapshot_v1(
    round: &ValidatedRecoverableVotingRoundV1,
    candidates: &[RecoverablePirSnapshotMetadataV1],
    match_index: u64,
) -> Result<VerifiedRecoverablePirSnapshotV1, VotingError> {
    let matches = candidates
        .iter()
        .filter(|candidate| {
            round.permits_pir_endpoint(candidate.endpoint())
                && candidate.snapshot_height() == round.snapshot_height()
                && candidate.snapshot_block_hash() == round.snapshot_block_hash()
                && candidate.pir_layout() == round.pir_layout()
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Err(invalid(
            "no exact PIR endpoint matched the verified round height, block hash, and layout",
        ));
    }
    let selected = matches[(match_index % matches.len() as u64) as usize];
    Ok(VerifiedRecoverablePirSnapshotV1 {
        round: round.clone(),
        metadata: selected.clone(),
    })
}

/// Blocking PIR client bound to an exact verified recoverable snapshot.
pub struct RecoverablePirClientV1 {
    client: PirClientBlocking,
    snapshot: VerifiedRecoverablePirSnapshotV1,
}

impl RecoverablePirClientV1 {
    pub fn endpoint(&self) -> &str {
        self.snapshot.endpoint()
    }

    pub fn snapshot_height(&self) -> u64 {
        self.snapshot.snapshot_height()
    }

    pub fn snapshot_block_hash(&self) -> &[u8; 32] {
        self.snapshot.snapshot_block_hash()
    }

    pub fn pir_layout(&self) -> PirLayout {
        self.snapshot.pir_layout()
    }

    pub(crate) fn validate_round(
        &self,
        round: &ValidatedRecoverableVotingRoundV1,
    ) -> Result<(), VotingError> {
        self.snapshot.validate_round(round)
    }

    pub(crate) fn inner(&self) -> &PirClientBlocking {
        &self.client
    }
}

/// Connects a blocking client to the exact endpoint and signed layout retained
/// by `snapshot`.
pub fn connect_recoverable_pir_blocking_v1(
    snapshot: &VerifiedRecoverablePirSnapshotV1,
    transport: Arc<dyn Transport>,
) -> Result<RecoverablePirClientV1, VotingError> {
    let client =
        crate::pir::connect_pir_blocking(snapshot.pir_layout(), snapshot.endpoint(), transport)?;
    Ok(RecoverablePirClientV1 {
        client,
        snapshot: snapshot.clone(),
    })
}

fn invalid(message: impl Into<String>) -> VotingError {
    VotingError::InvalidInput {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{AuthenticatedRound, ResolvedVotingConfig, ServiceEndpoint, SupportedVersions},
        recoverable_authority::{validate_recoverable_voting_round_v1, ChainVotingRoundV1},
        types::Network,
        wire::VotingRoundParams,
    };

    fn validated_round(round_id: [u8; 32]) -> ValidatedRecoverableVotingRoundV1 {
        let layout = PirLayout {
            pir_depth: 19,
            tier0_layers: 12,
            tier1_layers: 7,
            poly_len: 4096,
        };
        let config = ResolvedVotingConfig {
            source_fingerprint: "source".to_string(),
            trusted_key_fingerprint: "key".to_string(),
            dynamic_config_fingerprint: "dynamic".to_string(),
            vote_servers: vec![],
            pir_endpoints: vec![ServiceEndpoint {
                url: "https://pir.example.com".to_string(),
                label: "PIR".to_string(),
            }],
            pir_layout: layout,
            supported_versions: SupportedVersions {
                pir: vec!["v0".to_string()],
                vote_protocol: "v0".to_string(),
                tally: "v0".to_string(),
                vote_server: "v1".to_string(),
            },
            authenticated_rounds: vec![AuthenticatedRound {
                round_id: hex::encode(round_id),
                ea_pk: vec![0xEA; 32],
            }],
            skipped_round_ids: vec![],
            conditions: vec![],
        };
        let chain_round = ChainVotingRoundV1::new(
            VotingRoundParams {
                vote_round_id: hex::encode(round_id),
                snapshot_height: 1_234_567,
                ea_pk: vec![0xEA; 32],
                nc_root: vec![0x11; 32],
                nullifier_imt_root: vec![0x22; 32],
            },
            [0xAB; 32],
            3,
        )
        .unwrap();
        validate_recoverable_voting_round_v1(
            &config,
            Network::Testnet,
            "vote-chain-test",
            chain_round,
        )
        .unwrap()
    }

    fn exact_metadata(
        round: &ValidatedRecoverableVotingRoundV1,
    ) -> RecoverablePirSnapshotMetadataV1 {
        RecoverablePirSnapshotMetadataV1::new(
            "https://pir.example.com/",
            round.snapshot_height(),
            *round.snapshot_block_hash(),
            round.pir_layout(),
        )
        .unwrap()
    }

    #[test]
    fn exact_selector_rejects_cross_fork_and_layout_metadata() {
        let round = validated_round([0x01; 32]);
        let cross_fork = RecoverablePirSnapshotMetadataV1::new(
            "https://pir.example.com",
            round.snapshot_height(),
            [0xAC; 32],
            round.pir_layout(),
        )
        .unwrap();
        let cross_layout = RecoverablePirSnapshotMetadataV1::new(
            "https://pir.example.com",
            round.snapshot_height(),
            *round.snapshot_block_hash(),
            PirLayout {
                pir_depth: 19,
                tier0_layers: 11,
                tier1_layers: 8,
                poly_len: 2048,
            },
        )
        .unwrap();

        for candidate in [cross_fork, cross_layout] {
            let error = select_recoverable_pir_snapshot_v1(&round, &[candidate], 0)
                .err()
                .expect("mismatched exact PIR metadata must fail");
            assert!(matches!(error, VotingError::InvalidInput { .. }));
        }
    }

    #[test]
    fn verified_snapshot_rejects_a_different_validated_round() {
        let first_round = validated_round([0x01; 32]);
        let snapshot =
            select_recoverable_pir_snapshot_v1(&first_round, &[exact_metadata(&first_round)], 0)
                .unwrap();

        let second_round = validated_round([0x02; 32]);

        let error = snapshot
            .validate_round(&second_round)
            .err()
            .expect("cross-round PIR snapshot must fail before use");
        assert!(matches!(error, VotingError::InvalidInput { .. }));
    }
}
