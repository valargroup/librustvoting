use std::sync::Arc;

use crate::{
    chain_submission::{
        state::SubmissionObservation, ChainSubmissionConfirmationSource, ChainSubmissionDiagnostic,
        ChainSubmissionDiagnosticKind, ChainSubmissionState,
    },
    storage::queries,
    ChainSubmissionResult,
};

use super::{
    super::{
        sqlite::SqliteChainSubmissionStore, ChainSubmissionStore, ConfirmationCommit,
        StoreAdmission, StoreAdvancementRequest,
    },
    fixtures::{identity, open_prepared, temporary_path, ROUND},
};

#[test]
fn tree_confirmation_is_atomic_clamps_timestamp_and_survives_reopen_without_a_hash() {
    let path = temporary_path("tree-confirmed");
    let generation = {
        let db = open_prepared(&path);
        let store = SqliteChainSubmissionStore::new(db);
        let StoreAdmission::Ready { derived, .. } = store
            .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 10)
            .unwrap()
        else {
            panic!("fresh admission")
        };
        let generation = derived.generation().clone();
        store
            .classify_post(
                &generation,
                SubmissionObservation::PossiblyDispatched(
                    ChainSubmissionDiagnostic::from_redacted_message(
                        ChainSubmissionDiagnosticKind::AmbiguousDispatch,
                        "response lost",
                    ),
                ),
                11,
            )
            .unwrap();
        generation
    };
    {
        let db = open_prepared(&path);
        let store = SqliteChainSubmissionStore::new(Arc::clone(&db));
        let result = store
            .confirm_tree(
                &StoreAdvancementRequest::vote(identity()),
                &generation,
                7,
                vec![8],
                &|| true,
                5,
            )
            .unwrap();
        let ConfirmationCommit::Confirmed(record) = result else {
            panic!("tree confirmation must commit")
        };
        assert_eq!(record.updated_at(), 11);
        let confirmation = record.public_result().unwrap();
        let ChainSubmissionResult::Confirmed(confirmation) = confirmation else {
            panic!("tree confirmation must be terminal")
        };
        assert_eq!(
            confirmation.source(),
            ChainSubmissionConfirmationSource::Tree
        );
        assert_eq!(confirmation.transaction_hash(), None);
        assert_eq!(db.get_vote_tx_hash(ROUND, 0, 1).unwrap(), None);
        assert_eq!(
            queries::load_vote_row_state(&db.conn(), ROUND, "wallet", 0, 1)
                .unwrap()
                .unwrap()
                .vc_tree_position,
            Some(8)
        );
    }
    {
        let store = SqliteChainSubmissionStore::new(open_prepared(&path));
        let StoreAdmission::Authoritative(record) = store
            .admit(&StoreAdvancementRequest::vote(identity()), true, 1, 13)
            .unwrap()
        else {
            panic!("tree confirmation must survive reopen")
        };
        assert_eq!(record.durable_state(), ChainSubmissionState::Confirmed);
        assert_eq!(record.updated_at(), 11);
    }
    let _ = std::fs::remove_file(path);
}
