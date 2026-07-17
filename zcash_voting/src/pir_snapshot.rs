use serde::{Deserialize, Serialize};

use crate::{
    pir::pir_network,
    types::{Network, VotingError},
};
use pir_types::ZcashNetwork;

/// Normalized outcome for probing a PIR endpoint's `/root` snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PirSnapshotEndpointStatus {
    Matched,
    Behind,
    Ahead,
    MissingHeight,
    MissingNetwork,
    WrongNetwork,
    MalformedJson,
    NonSuccessStatus,
    TimeoutOrNetworkError,
}

/// Probe diagnostic for one configured PIR endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PirSnapshotEndpointDiagnostic {
    pub endpoint: String,
    pub status: PirSnapshotEndpointStatus,
    pub reported_height: Option<u64>,
    pub reported_network: Option<ZcashNetwork>,
    pub http_status_code: Option<u16>,
    pub message: Option<String>,
}

impl PirSnapshotEndpointDiagnostic {
    pub fn matched_at_height_and_network(
        &self,
        expected_snapshot_height: u64,
        expected_network: ZcashNetwork,
    ) -> bool {
        self.status == PirSnapshotEndpointStatus::Matched
            && self.reported_height == Some(expected_snapshot_height)
            && self.reported_network == Some(expected_network)
    }
}

/// Selected exact-snapshot PIR endpoint plus diagnostics for every probe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PirSnapshotResolution {
    pub endpoint: String,
    pub diagnostics: Vec<PirSnapshotEndpointDiagnostic>,
    pub selected_match_index: u64,
}

/// Build a normalized diagnostic from parsed `/root` fields.
pub fn classify_pir_snapshot_height(
    endpoint: impl Into<String>,
    expected_snapshot_height: u64,
    expected_network: Network,
    reported_height: Option<u64>,
    reported_network: Option<ZcashNetwork>,
) -> Result<PirSnapshotEndpointDiagnostic, VotingError> {
    let expected_network = pir_network(expected_network)?;
    let status = match (reported_network, reported_height) {
        (None, _) => PirSnapshotEndpointStatus::MissingNetwork,
        (Some(network), _) if network != expected_network => {
            PirSnapshotEndpointStatus::WrongNetwork
        }
        (_, Some(height)) if height == expected_snapshot_height => {
            PirSnapshotEndpointStatus::Matched
        }
        (_, Some(height)) if height < expected_snapshot_height => PirSnapshotEndpointStatus::Behind,
        (_, Some(_)) => PirSnapshotEndpointStatus::Ahead,
        (_, None) => PirSnapshotEndpointStatus::MissingHeight,
    };

    Ok(PirSnapshotEndpointDiagnostic {
        endpoint: endpoint.into(),
        status,
        reported_height,
        reported_network,
        http_status_code: None,
        message: None,
    })
}

/// Return endpoints that match the round height and network.
pub fn matching_pir_snapshot_endpoints(
    diagnostics: &[PirSnapshotEndpointDiagnostic],
    expected_snapshot_height: u64,
    expected_network: Network,
) -> Result<Vec<String>, VotingError> {
    let expected_network = pir_network(expected_network)?;
    Ok(diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.matched_at_height_and_network(expected_snapshot_height, expected_network)
        })
        .map(|diagnostic| diagnostic.endpoint.clone())
        .collect())
}

/// Select a PIR endpoint from normalized probe outcomes.
///
/// The selector only accepts exact height and network matches. `match_index` lets callers
/// inject their own random index while keeping the crate logic deterministic.
pub fn select_pir_snapshot_endpoint(
    diagnostics: &[PirSnapshotEndpointDiagnostic],
    expected_snapshot_height: u64,
    expected_network: Network,
    match_index: u64,
) -> Result<PirSnapshotResolution, VotingError> {
    let expected_pir_network = pir_network(expected_network)?;
    if diagnostics.is_empty() {
        return Err(VotingError::InvalidInput {
            message: "no PIR endpoints configured".to_string(),
        });
    }

    let matches =
        matching_pir_snapshot_endpoints(diagnostics, expected_snapshot_height, expected_network)?;
    if matches.is_empty() {
        return Err(VotingError::InvalidInput {
            message: format!(
                "no PIR endpoint matched snapshot height {} on {}",
                expected_snapshot_height, expected_pir_network
            ),
        });
    }

    let selected_match_index = match_index % matches.len() as u64;
    Ok(PirSnapshotResolution {
        endpoint: matches[selected_match_index as usize].clone(),
        diagnostics: diagnostics.to_vec(),
        selected_match_index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostic(
        endpoint: &str,
        status: PirSnapshotEndpointStatus,
        reported_height: Option<u64>,
        reported_network: Option<ZcashNetwork>,
    ) -> PirSnapshotEndpointDiagnostic {
        PirSnapshotEndpointDiagnostic {
            endpoint: endpoint.to_string(),
            status,
            reported_height,
            reported_network,
            http_status_code: None,
            message: None,
        }
    }

    #[test]
    fn classifies_parsed_heights_relative_to_expected_height() {
        assert_eq!(
            classify_pir_snapshot_height(
                "https://match.example.com",
                100,
                Network::Testnet,
                Some(100),
                Some(ZcashNetwork::Test),
            )
            .unwrap()
            .status,
            PirSnapshotEndpointStatus::Matched
        );
        assert_eq!(
            classify_pir_snapshot_height(
                "https://behind.example.com",
                100,
                Network::Testnet,
                Some(99),
                Some(ZcashNetwork::Test),
            )
            .unwrap()
            .status,
            PirSnapshotEndpointStatus::Behind
        );
        assert_eq!(
            classify_pir_snapshot_height(
                "https://ahead.example.com",
                100,
                Network::Testnet,
                Some(101),
                Some(ZcashNetwork::Test),
            )
            .unwrap()
            .status,
            PirSnapshotEndpointStatus::Ahead
        );
        assert_eq!(
            classify_pir_snapshot_height(
                "https://missing.example.com",
                100,
                Network::Testnet,
                None,
                Some(ZcashNetwork::Test),
            )
            .unwrap()
            .status,
            PirSnapshotEndpointStatus::MissingHeight
        );
    }

    #[test]
    fn classifies_missing_and_wrong_networks() {
        assert_eq!(
            classify_pir_snapshot_height(
                "https://missing.example.com",
                100,
                Network::Testnet,
                Some(100),
                None,
            )
            .unwrap()
            .status,
            PirSnapshotEndpointStatus::MissingNetwork
        );
        assert_eq!(
            classify_pir_snapshot_height(
                "https://main.example.com",
                100,
                Network::Testnet,
                Some(100),
                Some(ZcashNetwork::Main),
            )
            .unwrap()
            .status,
            PirSnapshotEndpointStatus::WrongNetwork
        );
    }

    #[test]
    fn selects_exact_height_match_by_injected_index() {
        let diagnostics = vec![
            diagnostic(
                "https://behind.example.com",
                PirSnapshotEndpointStatus::Behind,
                Some(99),
                Some(ZcashNetwork::Test),
            ),
            diagnostic(
                "https://one.example.com",
                PirSnapshotEndpointStatus::Matched,
                Some(100),
                Some(ZcashNetwork::Test),
            ),
            diagnostic(
                "https://two.example.com",
                PirSnapshotEndpointStatus::Matched,
                Some(100),
                Some(ZcashNetwork::Test),
            ),
        ];

        let resolution =
            select_pir_snapshot_endpoint(&diagnostics, 100, Network::Testnet, 5).unwrap();

        assert_eq!(resolution.endpoint, "https://two.example.com");
        assert_eq!(resolution.selected_match_index, 1);
        assert_eq!(resolution.diagnostics, diagnostics);
    }

    #[test]
    fn excludes_matched_status_without_exact_reported_height() {
        let diagnostics = vec![
            diagnostic(
                "https://missing-height.example.com",
                PirSnapshotEndpointStatus::Matched,
                None,
                Some(ZcashNetwork::Test),
            ),
            diagnostic(
                "https://wrong-height.example.com",
                PirSnapshotEndpointStatus::Matched,
                Some(101),
                Some(ZcashNetwork::Test),
            ),
        ];

        let err = select_pir_snapshot_endpoint(&diagnostics, 100, Network::Testnet, 0).unwrap_err();
        assert!(matches!(err, VotingError::InvalidInput { .. }));
    }

    #[test]
    fn excludes_same_height_endpoint_on_wrong_network() {
        let diagnostics = vec![diagnostic(
            "https://main.example.com",
            PirSnapshotEndpointStatus::Matched,
            Some(100),
            Some(ZcashNetwork::Main),
        )];

        let err = select_pir_snapshot_endpoint(&diagnostics, 100, Network::Testnet, 0).unwrap_err();
        assert!(err.to_string().contains("on test"));
    }

    #[test]
    fn errors_when_no_endpoints_are_configured() {
        let err = select_pir_snapshot_endpoint(&[], 100, Network::Testnet, 0).unwrap_err();
        assert!(matches!(err, VotingError::InvalidInput { .. }));
    }

    #[test]
    fn errors_when_no_endpoint_matches_exact_height() {
        let diagnostics = vec![
            diagnostic(
                "https://behind.example.com",
                PirSnapshotEndpointStatus::Behind,
                Some(99),
                Some(ZcashNetwork::Test),
            ),
            diagnostic(
                "https://ahead.example.com",
                PirSnapshotEndpointStatus::Ahead,
                Some(101),
                Some(ZcashNetwork::Test),
            ),
        ];

        let err = select_pir_snapshot_endpoint(&diagnostics, 100, Network::Testnet, 0).unwrap_err();
        assert!(matches!(err, VotingError::InvalidInput { .. }));
    }

    #[test]
    fn rejects_regtest_endpoint_selection() {
        let err = select_pir_snapshot_endpoint(&[], 100, Network::Regtest, 0).unwrap_err();
        assert!(err.to_string().contains("PIR does not support Regtest"));
    }
}
