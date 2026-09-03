//! Cross-handle lifecycle serialization for one durable SQLite authority.

use super::super::*;
use super::fixtures::*;
use crate::{chain_submission::coordination::CapturedSubmissionOperation, types::VotingError};

#[tokio::test]
async fn second_handle_cannot_delete_state_reserved_by_an_active_submission() {
    let path = temporary_path("shared-authority-deletion");
    assert_second_handle_cannot_delete_active_submission(&path).await;
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn shared_memory_handle_cannot_delete_state_reserved_by_an_active_submission() {
    let name = temporary_path("shared-memory-authority-deletion").replace(['/', '.'], "-");
    let uri = format!("file:{name}?mode=memory&cache=shared");
    assert_second_handle_cannot_delete_active_submission(&uri).await;
}

#[tokio::test]
async fn memdb_handle_cannot_delete_state_reserved_by_an_active_submission() {
    let name = temporary_path("memdb-authority-deletion").replace(['/', '.'], "-");
    let uri = format!("file:/{name}?vfs=memdb");
    assert_second_handle_cannot_delete_active_submission(&uri).await;
}

async fn assert_second_handle_cannot_delete_active_submission(path: &str) {
    let submitting_db = open_prepared(path);
    let deleting_db = Arc::new(VotingDb::open(path).unwrap());
    deleting_db.set_wallet_id("wallet");

    let store = SqliteChainSubmissionStore::new(Arc::clone(&submitting_db));
    let submission_identity = identity();
    let operation = CapturedSubmissionOperation::new(submission_identity.clone(), 0);
    // The coordinator holds this lease continuously from before admission until
    // the POST outcome is durable. Stop at that exact critical interval while
    // exercising deletion through an independently opened handle.
    let lifecycle_lease = store
        .coordination()
        .acquire(&operation, std::slice::from_ref(&submission_identity))
        .await
        .unwrap();
    assert!(matches!(
        store
            .admit(
                &StoreAdvancementRequest::vote(submission_identity),
                true,
                1,
                10,
            )
            .unwrap(),
        StoreAdmission::Ready {
            fresh_reservation: true,
            ..
        }
    ));

    assert!(matches!(
        deleting_db.clear_round(ROUND),
        Err(VotingError::Busy { .. })
    ));
    assert!(matches!(
        deleting_db.delete_skipped_bundles(ROUND, 0),
        Err(VotingError::Busy { .. })
    ));
    assert!(matches!(
        deleting_db.clear_wallet_state(),
        Err(VotingError::Busy { .. })
    ));

    let (submission_count, recovery_count): (u64, u64) = deleting_db
        .conn()
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM chain_submissions
                   WHERE round_id=?1 AND wallet_id='wallet'),
                 (SELECT COUNT(*) FROM votes
                   WHERE round_id=?1 AND wallet_id='wallet'
                     AND commitment_bundle_json IS NOT NULL)",
            [ROUND],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(submission_count, 1);
    assert_eq!(recovery_count, 1);
    assert!(deleting_db.has_round(ROUND).unwrap());

    drop(lifecycle_lease);
    deleting_db.clear_round(ROUND).unwrap();
    assert!(!deleting_db.has_round(ROUND).unwrap());

    drop((store, submitting_db, deleting_db));
}
