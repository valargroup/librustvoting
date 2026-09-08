use super::*;
use crate::{ObservabilityOptions, ObservationOutcome, ObservationScope};

/// Both setup layers must retain the reuse outcome and error category in the
/// emitted report, without changing the write-once result returned to callers.
#[test]
fn repeated_setup_reports_reuse_at_both_setup_layers() {
    let (db, _, _, prepared) = prepared_wallet_delegation_fixture();
    for expected in [ObservationOutcome::Succeeded, ObservationOutcome::Reused] {
        let invocation = ObservationScope::new(Some(ObservabilityOptions::default())).invocation();
        let setup =
            prepared.observe_setup(&db, &crate::types::NoopProgressReporter, invocation.scope());
        let expected_error_kind = if expected == ObservationOutcome::Succeeded {
            setup.unwrap();
            None
        } else {
            assert!(matches!(
                setup.unwrap_err(),
                VotingError::SetupAlreadyPersisted {
                    field: crate::types::DelegationSetupField::PcztSighash
                        | crate::types::DelegationSetupField::Tx1Effects,
                    ..
                }
            ));
            Some("SetupAlreadyPersisted")
        };
        let diagnostics = invocation.finish("setup", None, expected).unwrap();
        let setup_records: Vec<_> = diagnostics
            .records
            .iter()
            .filter(|record| record.stage.as_ref() == "delegation::setup")
            .collect();
        assert_eq!(setup_records.len(), 2, "both setup layers must be observed");
        assert_eq!(setup_records[1].parent_id, Some(setup_records[0].id));
        for record in setup_records {
            assert_eq!(record.outcome, expected);
            assert_eq!(record.error_kind.as_deref(), expected_error_kind);
            assert_eq!(record.attribution.bundle_index, Some(prepared.bundle_index));
        }
        assert_eq!(
            diagnostics.round_id.as_deref(),
            Some(prepared.round_id.as_str())
        );
    }
}
