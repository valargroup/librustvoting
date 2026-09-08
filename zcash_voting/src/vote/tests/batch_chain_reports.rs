//! Public batch reports retain transport diagnostics and durable lifecycle results.
use super::*;
use crate::{
    AdvanceVoteBatch, ChainHttpRequest, ChainHttpResponse, ChainSubmissionClient,
    ChainSubmissionClientConfig, ChainSubmissionControl, ChainSubmissionResult, ChainTransport,
    ChainTransportFuture, ObservabilityOptions, ObservationOutcome,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[derive(Default)]
struct BatchTransport {
    responses: Mutex<VecDeque<ChainHttpResponse>>,
    requests: Mutex<Vec<(String, Vec<u8>)>>,
}
impl ChainTransport for Arc<BatchTransport> {
    fn chain_get<'a>(&'a self, request: ChainHttpRequest) -> ChainTransportFuture<'a> {
        Box::pin(async move {
            self.requests
                .lock()
                .unwrap()
                .push((request.url().to_string(), vec![]));
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted GET"))
        })
    }
    fn chain_post_json<'a>(
        &'a self,
        request: ChainHttpRequest,
        json: Vec<u8>,
    ) -> ChainTransportFuture<'a> {
        Box::pin(async move {
            self.requests
                .lock()
                .unwrap()
                .push((request.url().to_string(), json));
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted POST"))
        })
    }
}

fn client(
    db: Arc<VotingDb>,
    transport: Arc<BatchTransport>,
) -> ChainSubmissionClient<Arc<BatchTransport>> {
    ChainSubmissionClient::with_transport(
        db,
        transport,
        ChainSubmissionClientConfig {
            network: Network::Testnet,
            vote_chain_id: "vote-test".into(),
            endpoints: vec!["https://vote.example".into()],
            tracking_window: Duration::from_secs(60),
            maximum_post_attempts: 1,
            retry_backoffs: vec![],
        },
    )
    .unwrap()
}

fn confirmed(digest: [u8; 32]) -> ChainHttpResponse {
    let attributes = [
        ("vote_round_id", ROUND_ID.to_string()),
        ("batch_digest", hex::encode(digest)),
        ("batch_size", "2".into()),
        ("final_van_leaf_index", "7".into()),
        ("vote_commitment_leaf_indices", "8,9".into()),
        ("proposal_ids", "1,2".into()),
        (
            "van_nullifiers",
            format!("{},{}", hex::encode([0x10; 32]), hex::encode([0x20; 32])),
        ),
    ]
    .into_iter()
    .map(|(key, value)| serde_json::json!({"key":key,"value":value}))
    .collect::<Vec<_>>();
    ChainHttpResponse::json(
        200,
        serde_json::to_vec(&serde_json::json!({
            "height":"9", "code":0, "log":"", "events":[{"type":"cast_vote_batch", "attributes":attributes}]
        }))
        .unwrap(),
    )
}

#[tokio::test]
async fn batch_reports_match_plain_calls_for_confirmation_pending_rejection_and_ambiguity() {
    for scenario in [
        "confirmed",
        "pending",
        "rejected",
        "missing_route",
        "cancelled",
        "invalid",
    ] {
        let mut baseline = None;
        for mode in 0..3 {
            let db = Arc::new(db_with_vote());
            let signed =
                persist_prepared_atomic_vote_batch(&db, prepared_atomic_vote_batch_fixture(&db))
                    .unwrap();
            let mut request = AdvanceVoteBatch {
                vote_round_id: hex::decode(ROUND_ID).unwrap().try_into().unwrap(),
                bundle_index: 0,
                ordered_batch_digest: signed.batch_digest,
                ordered_proposal_ids: vec![1, 2],
            };
            let transport = Arc::new(BatchTransport::default());
            let control = ChainSubmissionControl::new(1);
            match scenario {
                "cancelled" => control.cancel(),
                "invalid" => request.ordered_proposal_ids.clear(),
                "missing_route" => {
                    transport
                        .responses
                        .lock()
                        .unwrap()
                        .push_back(ChainHttpResponse::json(
                            404,
                            br#"{"error":"missing route"}"#.to_vec(),
                        ))
                }
                _ => {
                    let code = if scenario == "rejected" { 7 } else { 0 };
                    transport.responses.lock().unwrap().push_back(ChainHttpResponse::json(
                        if code == 0 { 200 } else { 422 },
                        serde_json::to_vec(&serde_json::json!({"tx_hash":HASH,"code":code,"batch_digest":hex::encode(signed.batch_digest)})).unwrap()));
                    if code == 0 {
                        transport
                            .responses
                            .lock()
                            .unwrap()
                            .push_back(if scenario == "confirmed" {
                                confirmed(signed.batch_digest)
                            } else {
                                ChainHttpResponse::json(
                                    404,
                                    br#"{"error":"tx not found"}"#.to_vec(),
                                )
                            });
                    }
                }
            }
            let client = client(Arc::clone(&db), Arc::clone(&transport));
            let (result, diagnostics) = if mode == 0 {
                (client.advance_vote_batch(request, &control).await, None)
            } else {
                client
                    .advance_vote_batch_with_report(
                        request,
                        &control,
                        (mode == 2).then(ObservabilityOptions::default),
                    )
                    .await
                    .into_parts()
            };
            let expected = match scenario {
                "confirmed" => {
                    assert!(
                        matches!(&result, Ok(ChainSubmissionResult::Confirmed(_))),
                        "{result:?}"
                    );
                    ObservationOutcome::Succeeded
                }
                "pending" => {
                    assert!(
                        matches!(&result, Ok(ChainSubmissionResult::Pending(_))),
                        "{result:?}"
                    );
                    ObservationOutcome::Pending
                }
                "rejected" => {
                    assert!(
                        matches!(&result, Ok(ChainSubmissionResult::Pending(crate::ChainSubmissionPending::Recovering { diagnostic, .. })) if diagnostic.kind() == crate::ChainSubmissionDiagnosticKind::ChainRejected),
                        "{result:?}"
                    );
                    ObservationOutcome::Pending
                }
                "missing_route" => {
                    // A router 404 never decoded the body: definitely unsent,
                    // no durable row, and a protocol failure naming the route.
                    assert!(
                        matches!(
                            &result,
                            Err(failure)
                                if failure.kind() == crate::ChainSubmissionFailureKind::Protocol
                                    && failure.strongest_state().is_none()
                                    && failure.message().contains("does not serve /shielded-vote/v1/cast-vote-batch")
                        ),
                        "{result:?}"
                    );
                    ObservationOutcome::Failed
                }
                "cancelled" => {
                    assert!(
                        matches!(&result, Ok(ChainSubmissionResult::Cancelled)),
                        "{result:?}"
                    );
                    ObservationOutcome::Cancelled
                }
                _ => {
                    assert!(result.is_err());
                    ObservationOutcome::Failed
                }
            };
            let hashes = [1, 2].map(|proposal| db.get_vote_tx_hash(ROUND_ID, 0, proposal).unwrap());
            let positions = [1, 2].map(|proposal| {
                queries::load_vote_row_state(&db.conn(), ROUND_ID, WALLET_ID, 0, proposal)
                    .unwrap()
                    .unwrap()
                    .vc_tree_position
            });
            let requests = transport.requests.lock().unwrap().clone();
            let effects = (hashes, positions, requests.clone());
            if let Some(baseline) = &baseline {
                assert_eq!(&effects, baseline);
            } else {
                baseline = Some(effects);
            }
            if scenario == "confirmed" {
                assert_eq!(positions, [Some(8), Some(9)]);
            }
            if matches!(scenario, "cancelled" | "invalid") {
                assert!(requests.is_empty());
            } else {
                assert_eq!(
                    requests[0].0,
                    "https://vote.example/shielded-vote/v1/cast-vote-batch"
                );
                assert_eq!(requests[0].1, signed.batch_json.as_bytes());
                assert_eq!(
                    requests.iter().filter(|(_, json)| !json.is_empty()).count(),
                    1
                );
            }
            if mode < 2 {
                assert!(diagnostics.is_none());
                continue;
            }
            let diagnostics = diagnostics.unwrap();
            assert_eq!(diagnostics.outcome, expected);
            assert_eq!(diagnostics.round_id.as_deref(), Some(ROUND_ID));
            assert!(diagnostics
                .records
                .iter()
                .all(|record| record.attribution.bundle_index == Some(0)
                    && record.attribution.proposal_id.is_none()
                    && record.attribution.share_index.is_none()));
            for record in &diagnostics.records {
                if let Some(parent) = record.parent_id {
                    assert!(diagnostics.records.iter().any(|r| r.id == parent));
                }
            }
            if !requests.is_empty() {
                assert!(diagnostics.records.iter().any(|r| r.http_status.is_some()
                    && r.attempt == Some(1)
                    && r.endpoint_index == Some(0)));
            }
            // A rejected POST and an ambiguous POST both require recovery, but
            // the transport observations must preserve their different evidence.
            if scenario == "rejected" {
                assert!(diagnostics
                    .records
                    .iter()
                    .any(|r| r.outcome == ObservationOutcome::Rejected));
            }
            if scenario == "missing_route" {
                assert!(diagnostics
                    .records
                    .iter()
                    .any(|r| r.outcome == ObservationOutcome::Failed
                        && r.error_kind.as_deref() == Some("EndpointUnsupported")));
            }
            let serialized = serde_json::to_string(&diagnostics).unwrap();
            assert!(!serialized.contains("vote.example"));
            assert!(!serialized.contains("missing route"));
            assert!(!serialized.contains(&signed.batch_json));
        }
    }
}

#[tokio::test]
async fn round_report_keeps_batch_reconciliation_under_the_triggering_step() {
    for options in [None, Some(ObservabilityOptions::default())] {
        let db = Arc::new(db_with_vote());
        let signed =
            persist_prepared_atomic_vote_batch(&db, prepared_atomic_vote_batch_fixture(&db))
                .unwrap();
        let transport = Arc::new(BatchTransport::default());
        transport.responses.lock().unwrap().extend([
            ChainHttpResponse::json(
                200,
                serde_json::to_vec(&serde_json::json!({
                    "tx_hash": HASH, "code": 0, "batch_digest": hex::encode(signed.batch_digest)
                }))
                .unwrap(),
            ),
            ChainHttpResponse::json(404, br#"{"error":"tx not found"}"#.to_vec()),
        ]);
        let initial = client(Arc::clone(&db), Arc::clone(&transport))
            .advance_vote_batch(
                AdvanceVoteBatch {
                    vote_round_id: hex::decode(ROUND_ID).unwrap().try_into().unwrap(),
                    bundle_index: 0,
                    ordered_batch_digest: signed.batch_digest,
                    ordered_proposal_ids: vec![1, 2],
                },
                &ChainSubmissionControl::new(1),
            )
            .await
            .unwrap();
        assert!(matches!(initial, ChainSubmissionResult::Pending(_)));
        transport.requests.lock().unwrap().clear();
        transport
            .responses
            .lock()
            .unwrap()
            .push_back(ChainHttpResponse::json(
                422,
                br#"{"height":"9","code":12,"log":"rejected","events":[]}"#.to_vec(),
            ));
        let executor = crate::RoundExecutor::with_transport(
            Arc::clone(&db),
            Arc::clone(&transport),
            ChainSubmissionClientConfig {
                network: Network::Testnet,
                vote_chain_id: "vote-test".into(),
                endpoints: vec!["https://vote.example".into()],
                tracking_window: Duration::from_secs(60),
                maximum_post_attempts: 1,
                retry_backoffs: vec![],
            },
            crate::HelperClient::new(
                Arc::new(crate::HyperTransport::new()),
                crate::HelperHealth::default(),
            ),
        )
        .unwrap()
        .with_binding(crate::RoundBinding {
            round_id: ROUND_ID.into(),
            network: Network::Testnet,
            proposals: [1, 2]
                .into_iter()
                .map(|proposal_id| crate::ProposalRosterEntry {
                    proposal_id,
                    num_options: 3,
                })
                .collect(),
            hotkey_secret: None,
        })
        .unwrap();
        let host = crate::round_drive::RoundHostSourceBridge::new(|| crate::RoundHostContext {
            configured_helper_urls: vec!["http://helper.invalid".into()],
            now_seconds: 10,
            ceremony_start_seconds: Some(0),
            vote_end_time_seconds: Some(100_000),
            vote_tree_node_urls: vec![],
            delegation: None,
            chain_policy: crate::ChainAdvancePolicy {
                max_passes: 1,
                ..Default::default()
            },
            max_proof_concurrency: 1,
        });
        let report = crate::RoundDriver::new(&executor)
            .run_with_report(
                &host,
                &ChainSubmissionControl::new(1),
                &crate::round_drive::NoopRoundDriveReporter {},
                options,
            )
            .await;
        assert!(report.result.failures.is_empty(), "{:?}", report.result);
        assert!(
            matches!(
                report.result.quiescence,
                crate::RoundQuiescence::ChainRecoveryStalled { .. }
            ),
            "{:?}",
            report.result
        );
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].1.is_empty(),
            "resumed batch is polled without another POST"
        );
        if options.is_none() {
            assert!(report.observability.is_none());
            continue;
        }
        let diagnostics = report.observability.unwrap();
        assert_eq!(diagnostics.outcome, ObservationOutcome::Pending);
        let step = diagnostics
            .records
            .iter()
            .find(|r| r.stage.as_ref() == "round::advance_step")
            .unwrap();
        assert_eq!(step.attribution.proposal_id, Some(1));
        let batch_records = diagnostics
            .records
            .iter()
            .filter(|r| {
                matches!(
                    r.stage.as_ref(),
                    "vote::recover_atomic_vote_batch" | "chain::advance_until_terminal_in_epoch"
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(batch_records.len(), 2);
        for record in batch_records {
            assert_eq!(record.attribution.bundle_index, Some(0));
            assert_eq!(record.attribution.proposal_id, None);
            assert_eq!(record.attribution.share_index, None);
            let mut parent = record.parent_id;
            while parent != Some(step.id) {
                parent = diagnostics
                    .records
                    .iter()
                    .find(|r| Some(r.id) == parent)
                    .expect("batch descendant of step")
                    .parent_id;
            }
        }
    }
}
