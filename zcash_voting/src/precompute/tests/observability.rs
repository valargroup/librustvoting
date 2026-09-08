use super::{precompute_pir_proofs, precompute_pir_proofs_with_report};
use crate::{
    backend::pasta_curves::pallas::Base, pir::PirProofSource, round::VotingDb, BundlePolicy,
    Network, NoteInfo, ObservabilityOptions, ObservationOutcome,
};
use std::sync::atomic::{AtomicUsize, Ordering};

struct FailingPir {
    fetches: AtomicUsize,
}
impl PirProofSource for FailingPir {
    fn circuit_root(&self) -> Base {
        Base::from(0)
    }
    fn fetch_proofs(&self, _: &[Base]) -> anyhow::Result<Vec<pir_client::ImtProofData>> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("test PIR unavailable")
    }
}

#[test]
fn warm_cache_reports_empty_success_and_actual_fetch_failure() {
    let db = VotingDb::open_in_memory().unwrap();
    db.set_wallet_id("observed-warm-cache");
    let pir = FailingPir {
        fetches: AtomicUsize::new(0),
    };
    let note = NoteInfo {
        commitment: vec![1; 32],
        nullifier: vec![2; 32],
        value: crate::governance::BALLOT_DIVISOR,
        position: 0,
        diversifier: vec![3; 11],
        rho: vec![4; 32],
        rseed: vec![5; 32],
        scope: 0,
        ufvk_str: "uviewtest".into(),
    };
    for options in [None, Some(ObservabilityOptions::default())] {
        let report = precompute_pir_proofs_with_report(
            &db,
            &[],
            BundlePolicy::default(),
            Network::Testnet,
            &pir,
            options,
        );
        assert_eq!(report.result.unwrap().fetched_count, 0);
        assert_eq!(report.observability.is_some(), options.is_some());
        if let Some(diagnostics) = report.observability {
            assert_eq!(diagnostics.outcome, ObservationOutcome::Succeeded);
        }
        let report = precompute_pir_proofs_with_report(
            &db,
            std::slice::from_ref(&note),
            BundlePolicy::default(),
            Network::Testnet,
            &pir,
            options,
        );
        let plain = precompute_pir_proofs(
            &db,
            std::slice::from_ref(&note),
            BundlePolicy::default(),
            Network::Testnet,
            &pir,
        )
        .unwrap_err();
        assert_eq!(report.result.unwrap_err().to_string(), plain.to_string());
        assert_eq!(report.observability.is_some(), options.is_some());
        if let Some(diagnostics) = report.observability {
            assert_eq!(diagnostics.outcome, ObservationOutcome::Failed);
            assert!(diagnostics.round_id.is_none());
            assert_eq!(diagnostics.records[0].outcome, ObservationOutcome::Failed);
            assert!(diagnostics.records[0].error_kind.is_some());
        }
    }
    assert_eq!(pir.fetches.load(Ordering::SeqCst), 4);
}
