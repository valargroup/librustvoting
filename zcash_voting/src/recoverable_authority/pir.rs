//! Exact-snapshot PIR binding for recoverable voting authority.

use std::sync::Arc;

use crate::{
    config::{PirLayout, VerifiedRoundAuthV3, VerifiedVotingRoundV3},
    pir::{PirClientBlocking, Transport},
    types::VotingError,
};

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

/// Exact PIR snapshot selected against one trusted-key-verified round.
///
/// This token is opaque and not serializable. It therefore cannot be rebuilt
/// from the legacy height-only [`crate::pir_snapshot::PirSnapshotResolution`].
#[derive(Clone, PartialEq, Eq)]
pub struct VerifiedRecoverablePirSnapshotV1 {
    round_auth: VerifiedRoundAuthV3,
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

    fn validate_round(&self, round: &VerifiedVotingRoundV3) -> Result<(), VotingError> {
        if &self.round_auth != round.round_auth()
            || self.snapshot_height() != round.round_params().snapshot_height
            || self.snapshot_height() != round.round_auth().context().snapshot_height()
            || self.snapshot_block_hash() != round.round_auth().context().snapshot_block_hash()
            || self.pir_layout() != round.round_auth().pir_layout()
            || !round.round_auth().permits_pir_endpoint(self.endpoint())
        {
            return Err(invalid(
                "recoverable PIR client does not match the verified voting round",
            ));
        }
        Ok(())
    }
}

/// Selects exact metadata that matches one verified v3 round in every field.
///
/// `match_index` lets callers inject deterministic or random selection without
/// turning endpoint probing into part of this API.
pub fn select_recoverable_pir_snapshot_v1(
    round_auth: &VerifiedRoundAuthV3,
    candidates: &[RecoverablePirSnapshotMetadataV1],
    match_index: u64,
) -> Result<VerifiedRecoverablePirSnapshotV1, VotingError> {
    let matches = candidates
        .iter()
        .filter(|candidate| {
            round_auth.permits_pir_endpoint(candidate.endpoint())
                && candidate.snapshot_height() == round_auth.context().snapshot_height()
                && candidate.snapshot_block_hash() == round_auth.context().snapshot_block_hash()
                && candidate.pir_layout() == round_auth.pir_layout()
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Err(invalid(
            "no exact PIR endpoint matched the verified round height, block hash, and layout",
        ));
    }
    let selected = matches[(match_index % matches.len() as u64) as usize];
    Ok(VerifiedRecoverablePirSnapshotV1 {
        round_auth: round_auth.clone(),
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

    pub(crate) fn validate_round(&self, round: &VerifiedVotingRoundV3) -> Result<(), VotingError> {
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
        recoverable_authority::{test_verified_round_auth_v3, VotingAuthorityContextV1},
        types::Network,
        wire::VotingRoundParams,
    };

    fn authority_context(round_id: [u8; 32]) -> VotingAuthorityContextV1 {
        VotingAuthorityContextV1::from_fingerprint(
            Network::Testnet,
            0,
            [0x44; 32],
            "vote-chain-test",
            round_id,
        )
        .unwrap()
    }

    fn exact_metadata(round_auth: &VerifiedRoundAuthV3) -> RecoverablePirSnapshotMetadataV1 {
        RecoverablePirSnapshotMetadataV1::new(
            "https://pir.example.com/",
            round_auth.context().snapshot_height(),
            *round_auth.context().snapshot_block_hash(),
            round_auth.pir_layout(),
        )
        .unwrap()
    }

    #[test]
    fn exact_selector_rejects_cross_fork_and_layout_metadata() {
        let round_auth = test_verified_round_auth_v3(&authority_context([0x01; 32]));
        let cross_fork = RecoverablePirSnapshotMetadataV1::new(
            "https://pir.example.com",
            round_auth.context().snapshot_height(),
            [0xAC; 32],
            round_auth.pir_layout(),
        )
        .unwrap();
        let cross_layout = RecoverablePirSnapshotMetadataV1::new(
            "https://pir.example.com",
            round_auth.context().snapshot_height(),
            *round_auth.context().snapshot_block_hash(),
            PirLayout {
                pir_depth: 19,
                tier0_layers: 11,
                tier1_layers: 8,
                poly_len: 2048,
            },
        )
        .unwrap();

        for candidate in [cross_fork, cross_layout] {
            let error = select_recoverable_pir_snapshot_v1(&round_auth, &[candidate], 0)
                .err()
                .expect("mismatched exact PIR metadata must fail");
            assert!(matches!(error, VotingError::InvalidInput { .. }));
        }
    }

    #[test]
    fn verified_snapshot_rejects_a_different_verified_round() {
        let first_auth = test_verified_round_auth_v3(&authority_context([0x01; 32]));
        let snapshot =
            select_recoverable_pir_snapshot_v1(&first_auth, &[exact_metadata(&first_auth)], 0)
                .unwrap();

        let second_auth = test_verified_round_auth_v3(&authority_context([0x02; 32]));
        let second_round = crate::config::test_verified_voting_round_v3(
            &second_auth,
            VotingRoundParams {
                vote_round_id: hex::encode(second_auth.round_id()),
                snapshot_height: second_auth.context().snapshot_height(),
                ea_pk: second_auth.ea_pk().to_vec(),
                nc_root: vec![0x11; 32],
                nullifier_imt_root: vec![0x22; 32],
            },
        );

        let error = snapshot
            .validate_round(&second_round)
            .err()
            .expect("cross-round PIR snapshot must fail before use");
        assert!(matches!(error, VotingError::InvalidInput { .. }));
    }
}
