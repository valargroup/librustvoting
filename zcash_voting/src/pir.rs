//! PIR endpoint selection and client re-exports.
//!
//! Wallets use this module to select an exact PIR snapshot endpoint
//! before delegation PIR precomputation.

pub use crate::pir_snapshot::{
    classify_pir_snapshot_height, matching_pir_snapshot_endpoints, select_pir_snapshot_endpoint,
    PirSnapshotEndpointDiagnostic, PirSnapshotEndpointStatus, PirSnapshotResolution,
};

/// Candidate PIR endpoint URL.
pub type PirEndpoint = String;

/// Maps a voting network to the public Zcash network used by PIR.
pub fn pir_network(
    network: crate::types::Network,
) -> Result<ZcashNetwork, crate::types::VotingError> {
    match network {
        crate::types::Network::Mainnet => Ok(ZcashNetwork::Main),
        crate::types::Network::Testnet => Ok(ZcashNetwork::Test),
        crate::types::Network::Regtest => Err(crate::types::VotingError::InvalidInput {
            message: "PIR does not support Regtest".to_string(),
        }),
    }
}

/// Selects an exact snapshot endpoint from already-probed diagnostics.
///
/// `match_index` lets callers inject deterministic or random selection without
/// making endpoint probing part of the core API.
pub fn select_pir_endpoint(
    diagnostics: &[PirSnapshotEndpointDiagnostic],
    snapshot_height: u64,
    network: crate::types::Network,
    match_index: u64,
) -> Result<PirSnapshotResolution, crate::types::VotingError> {
    select_pir_snapshot_endpoint(diagnostics, snapshot_height, network, match_index)
}

pub use pir_client::{
    ImtProofData, PirClient, PirClientBlocking, Transport, TransportFuture, TransportResponse,
    ZcashNetwork,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Network;

    #[test]
    fn maps_public_zcash_networks_for_pir() {
        assert_eq!(pir_network(Network::Mainnet).unwrap(), ZcashNetwork::Main);
        assert_eq!(pir_network(Network::Testnet).unwrap(), ZcashNetwork::Test);
    }

    #[test]
    fn rejects_regtest_for_pir() {
        let err = pir_network(Network::Regtest).unwrap_err();
        assert!(err.to_string().contains("PIR does not support Regtest"));
    }
}
