use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use rusqlite::named_params;

use crate::{
    chain_submission::{
        AdvanceImportedDelegation, ChainHttpRequest, ChainHttpResponse, ChainSubmissionClient,
        ChainSubmissionClientConfig, ChainSubmissionControl, ChainSubmissionFailureKind,
        ChainSubmissionResult, ChainTransport, ChainTransportError, ChainTransportFuture,
    },
    session::{resume_plan, NextStep},
    storage::{queries, VotingDb},
    Network, VotingRoundParams,
};

const ROUND: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const WALLET: &str = "wallet";

#[derive(Default)]
struct PollOnlyTransport {
    responses: Mutex<VecDeque<Result<ChainHttpResponse, ChainTransportError>>>,
    get_count: Mutex<usize>,
    post_count: Mutex<usize>,
}

impl PollOnlyTransport {
    fn queue_json(&self, status: u16, body: impl Into<Vec<u8>>) {
        self.responses
            .lock()
            .unwrap()
            .push_back(Ok(ChainHttpResponse::json(status, body.into())));
    }
}

impl ChainTransport for Arc<PollOnlyTransport> {
    fn chain_get<'a>(&'a self, _request: ChainHttpRequest) -> ChainTransportFuture<'a> {
        Box::pin(async move {
            *self.get_count.lock().unwrap() += 1;
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted imported-delegation response")
        })
    }

    fn chain_post_json<'a>(
        &'a self,
        _request: ChainHttpRequest,
        _json: Vec<u8>,
    ) -> ChainTransportFuture<'a> {
        Box::pin(async move {
            *self.post_count.lock().unwrap() += 1;
            panic!("imported delegation advancement must never POST")
        })
    }
}

fn imported_db() -> Arc<VotingDb> {
    let db = Arc::new(VotingDb::open_in_memory().unwrap());
    db.set_wallet_id(WALLET);
    db.create_round(
        Network::Testnet,
        &VotingRoundParams {
            vote_round_id: ROUND.to_string(),
            snapshot_height: 100,
            ea_pk: vec![0xea; 32],
            nc_root: vec![0xaa; 32],
            nullifier_imt_root: vec![0xbb; 32],
        },
        None,
    )
    .unwrap();
    db.conn()
        .execute(
            "INSERT INTO bundles
             (round_id, wallet_id, bundle_index, van_comm_rand, gov_comm,
              total_note_value, address_index, delegation_tx_hash)
             VALUES (:round, :wallet, 0, :randomizer, :commitment, 100000000, 0, :hash)",
            named_params! {
                ":round": ROUND,
                ":wallet": WALLET,
                ":randomizer": vec![0x21_u8; 32],
                ":commitment": vec![0x31_u8; 32],
                ":hash": HASH,
            },
        )
        .unwrap();
    db
}

fn client(
    db: Arc<VotingDb>,
    transport: Arc<PollOnlyTransport>,
) -> ChainSubmissionClient<Arc<PollOnlyTransport>> {
    ChainSubmissionClient::with_transport(
        db,
        transport,
        ChainSubmissionClientConfig {
            network: Network::Testnet,
            vote_chain_id: "vote-test".to_string(),
            endpoints: vec!["https://vote.example".to_string()],
            tracking_window: Duration::from_secs(60),
            maximum_post_attempts: 1,
            retry_backoffs: vec![],
        },
    )
    .unwrap()
}

fn request() -> AdvanceImportedDelegation {
    AdvanceImportedDelegation {
        vote_round_id: hex::decode(ROUND).unwrap().try_into().unwrap(),
        bundle_index: 0,
    }
}

#[tokio::test]
async fn imported_delegation_is_adopted_once_and_only_polled() {
    let db = imported_db();
    assert_eq!(
        resume_plan(&db, ROUND, &[1]).unwrap().next_steps,
        vec![NextStep::AdvanceImportedDelegation { bundle_index: 0 }]
    );
    let transport = Arc::new(PollOnlyTransport::default());
    transport.queue_json(404, br#"{"error":"tx not found"}"#.to_vec());
    transport.queue_json(404, br#"{"error":"tx not found"}"#.to_vec());
    let client = client(Arc::clone(&db), Arc::clone(&transport));

    for _ in 0..2 {
        assert!(matches!(
            client
                .advance_imported_delegation(request(), &ChainSubmissionControl::new(1))
                .await
                .unwrap(),
            ChainSubmissionResult::Pending(_)
        ));
    }

    assert_eq!(*transport.get_count.lock().unwrap(), 2);
    assert_eq!(*transport.post_count.lock().unwrap(), 0);
    let (rows, state, attempts, candidate): (i64, String, i64, Vec<u8>) = db
        .conn()
        .query_row(
            "SELECT COUNT(*), state, committed_post_reservations,
                    candidate_transaction_hash
             FROM chain_submissions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!((rows, state.as_str(), attempts), (1, "tracking", 0));
    assert_eq!(hex::encode(candidate), HASH);
    assert_eq!(
        resume_plan(&db, ROUND, &[1]).unwrap().next_steps,
        vec![NextStep::AdvanceImportedDelegation { bundle_index: 0 }]
    );
}

#[tokio::test]
async fn imported_delegation_confirmation_is_atomic_and_signer_free() {
    let db = imported_db();
    let transport = Arc::new(PollOnlyTransport::default());
    transport.queue_json(
        200,
        format!(
            r#"{{"height":"42","code":0,"log":"","events":[{{"type":"delegate_vote","attributes":[{{"key":"vote_round_id","value":"{ROUND}","index":true}},{{"key":"leaf_index","value":"7","index":true}}]}}]}}"#
        ),
    );

    let result = client(Arc::clone(&db), Arc::clone(&transport))
        .advance_imported_delegation(request(), &ChainSubmissionControl::new(1))
        .await
        .unwrap();

    let ChainSubmissionResult::Confirmed(confirmation) = result else {
        panic!("expected imported delegation confirmation")
    };
    assert_eq!(confirmation.final_van_position(), 7);
    assert_eq!(db.load_van_position_u64(ROUND, 0).unwrap(), 7);
    assert_eq!(
        db.get_delegation_tx_hash(ROUND, 0).unwrap().as_deref(),
        Some(HASH)
    );
    assert_eq!(*transport.post_count.lock().unwrap(), 0);
    let state: String = db
        .conn()
        .query_row("SELECT state FROM chain_submissions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(state, "confirmed");
}

#[tokio::test]
async fn failed_import_in_recovery_becomes_terminal_instead_of_hashless_recovery() {
    let db = imported_db();
    let transport = Arc::new(PollOnlyTransport::default());
    transport.queue_json(404, br#"{"error":"tx not found"}"#.to_vec());
    let client = client(Arc::clone(&db), Arc::clone(&transport));
    assert!(matches!(
        client
            .advance_imported_delegation(request(), &ChainSubmissionControl::new(1))
            .await
            .unwrap(),
        ChainSubmissionResult::Pending(_)
    ));
    db.conn()
        .execute(
            "UPDATE chain_submissions
             SET state = 'recovering', diagnostic_kind = 'tracking_window_expired',
                 diagnostic = 'tracking expired'",
            [],
        )
        .unwrap();
    transport.queue_json(
        422,
        br#"{"height":"9","code":12,"log":"rejected","events":[]}"#.to_vec(),
    );

    let result = client
        .advance_imported_delegation(request(), &ChainSubmissionControl::new(1))
        .await
        .unwrap();

    assert!(
        matches!(result, ChainSubmissionResult::Rejected(_)),
        "unexpected result: {result:?}"
    );
    assert_eq!(*transport.post_count.lock().unwrap(), 0);
    let state: String = db
        .conn()
        .query_row("SELECT state FROM chain_submissions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(state, "rejected");
    assert_eq!(
        resume_plan(&db, ROUND, &[1]).unwrap().next_steps,
        Vec::<NextStep>::new()
    );
}

#[tokio::test]
async fn imported_advancement_rejects_a_locally_prepared_bundle_without_network_io() {
    let db = Arc::new(VotingDb::open_in_memory().unwrap());
    db.set_wallet_id(WALLET);
    db.create_round(
        Network::Testnet,
        &VotingRoundParams {
            vote_round_id: ROUND.to_string(),
            snapshot_height: 100,
            ea_pk: vec![0xea; 32],
            nc_root: vec![0xaa; 32],
            nullifier_imt_root: vec![0xbb; 32],
        },
        None,
    )
    .unwrap();
    queries::insert_bundle(&db.conn(), ROUND, WALLET, 0, &[1]).unwrap();
    let transport = Arc::new(PollOnlyTransport::default());

    let error = client(db, Arc::clone(&transport))
        .advance_imported_delegation(request(), &ChainSubmissionControl::new(1))
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ChainSubmissionFailureKind::InvalidInput);
    assert!(
        error.message().contains("imported delegation"),
        "unexpected error: {error:?}"
    );
    assert_eq!(*transport.get_count.lock().unwrap(), 0);
    assert_eq!(*transport.post_count.lock().unwrap(), 0);
}

#[tokio::test]
async fn malformed_imported_bundle_column_reports_a_storage_failure() {
    let db = imported_db();
    db.conn()
        .execute(
            "UPDATE bundles SET gov_comm = 7
             WHERE round_id = :round AND wallet_id = :wallet AND bundle_index = 0",
            named_params! { ":round": ROUND, ":wallet": WALLET },
        )
        .unwrap();
    let transport = Arc::new(PollOnlyTransport::default());

    let error = client(db, Arc::clone(&transport))
        .advance_imported_delegation(request(), &ChainSubmissionControl::new(1))
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ChainSubmissionFailureKind::Storage);
    assert!(
        error
            .message()
            .contains("failed to read imported delegation capability bundle"),
        "unexpected error: {error:?}"
    );
    assert_eq!(*transport.get_count.lock().unwrap(), 0);
    assert_eq!(*transport.post_count.lock().unwrap(), 0);
}
