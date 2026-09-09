use super::*;
use crate::{ObservabilityOptions, ObservationOutcome as Outcome};

fn delivery_report(evidence: &[(bool, bool)]) -> ShareBatchDeliveryReport {
    ShareBatchDeliveryReport {
        deliveries: evidence
            .iter()
            .enumerate()
            .map(|(index, &(accepted, ambiguous))| ShareDeliveryOutcome {
                share_index: index as u32,
                submission: ShareSubmissionReport {
                    accepted_urls: if accepted { vec![helper(1)] } else { vec![] },
                    ambiguous_urls: if ambiguous { vec![helper(2)] } else { vec![] },
                    target_count: 2,
                },
            })
            .collect(),
        pending_share_indices: vec![],
        cancelled: false,
        placement_guarantee: SharePlacementGuarantee::Strict,
    }
}

#[test]
fn delivery_diagnostics_classify_evidence_and_boundary_precedence() {
    use crate::observability::share_delivery_outcome;
    for (evidence, expected) in [
        (vec![], Outcome::Succeeded),
        (vec![(true, false), (true, true)], Outcome::Succeeded),
        (vec![(false, true), (false, true)], Outcome::Pending),
        (vec![(true, false), (false, true)], Outcome::Pending),
        (vec![(false, false), (false, false)], Outcome::Failed),
        (vec![(true, true), (false, false)], Outcome::Failed),
        (vec![(false, true), (false, false)], Outcome::Failed),
    ] {
        assert_eq!(
            share_delivery_outcome(&Ok(delivery_report(&evidence))),
            expected
        );
    }
    let mut report = delivery_report(&[(false, false)]);
    report.pending_share_indices.push(1);
    assert_eq!(
        share_delivery_outcome(&Ok(report.clone())),
        Outcome::Pending
    );
    report.cancelled = true;
    assert_eq!(share_delivery_outcome(&Ok(report)), Outcome::Cancelled);
    assert_eq!(
        share_delivery_outcome(&Err(VotingError::Internal {
            message: "journal failed".to_string(),
        })),
        Outcome::Failed,
    );
}

/// Complete, valid share payloads without invoking the prover.
pub(super) fn complete_recovery(proposal_id: u32) -> VoteRecoveryBundle {
    let mut recovery = recovery_bundle_fixture();
    recovery.proposal_id = proposal_id;
    recovery.encrypted_shares = (0..crate::share_policy::VOTE_COMMITMENT_SHARE_COUNT)
        .map(|index| EncryptedShare {
            c1: point_bytes(index as u64 * 2 + 1),
            c2: point_bytes(index as u64 * 2 + 2),
            share_index: index as u32,
            plaintext_value: index as u64 + 1,
            randomness: vec![index as u8 + 1; 32],
        })
        .collect();
    recovery.share_blinds = (0..crate::share_policy::VOTE_COMMITMENT_SHARE_COUNT)
        .map(|index| field_bytes(index as u8 + 1))
        .collect();
    recovery
}

pub(super) fn store_recovery(db: &VotingDb, recovery: &VoteRecoveryBundle, confirmed: bool) {
    db.set_ballot_intent(
        ROUND_ID,
        recovery.proposal_id,
        crate::session::Decision::Choice(2),
        3,
    )
    .unwrap();
    queries::store_vote(
        &db.conn(),
        ROUND_ID,
        &db.wallet_id(),
        0,
        recovery.proposal_id,
        2,
        &crate::vote::stored_vote_commitment_bytes(recovery).unwrap(),
    )
    .unwrap();
    db.conn()
        .execute(
            "UPDATE votes SET commitment_bundle_json = :json, vc_tree_position = :position
         WHERE round_id = :round AND wallet_id = :wallet AND bundle_index = 0
           AND proposal_id = :proposal",
            rusqlite::named_params! {
                ":json": serialize_recovery(recovery).unwrap(),
                ":position": confirmed.then_some(recovery.vc_tree_position as i64),
                ":round": ROUND_ID, ":wallet": db.wallet_id(), ":proposal": recovery.proposal_id,
            },
        )
        .unwrap();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReportingMode {
    Plain,
    Disabled,
    Enabled,
}

#[tokio::test]
async fn reported_delivery_preserves_results_and_classifies_completed_tasks() {
    // Each mode gets an equivalent fresh database: a prior pass must not
    // change the work performed by the next comparison.
    for expected in [Outcome::Succeeded, Outcome::Pending, Outcome::Failed] {
        let mut baseline = None;
        for mode in [
            ReportingMode::Plain,
            ReportingMode::Disabled,
            ReportingMode::Enabled,
        ] {
            let db = db_with_round_and_bundle();
            store_recovery(&db, &complete_recovery(1), true);
            let configured = helpers(1);
            let fleet = HelperFleetPreflight::from_readiness(&configured, &configured).unwrap();
            let vote = crate::vote::CommittedVote::recover(&db, ROUND_ID, 0, 1).unwrap();
            vote.prepare_share_delivery(
                &db,
                ShareDeliveryPlanningParams {
                    fleet: &fleet,
                    now_seconds: SUBMIT_AT,
                    vote_end_time_seconds: VOTE_END,
                    last_moment_buffer_seconds: None,
                    proposal_ids: &[1],
                },
            )
            .unwrap();
            let vote = vote.confirmed(&db).unwrap().unwrap();
            let transport = Arc::new(MockTransport::default());
            let post_url = format!("{}/shielded-vote/v1/shares", configured[0]);
            for _ in 0..crate::share_policy::VOTE_COMMITMENT_SHARE_COUNT {
                transport.queue_post(
                    &post_url,
                    match expected {
                        Outcome::Succeeded => json_status("queued"),
                        Outcome::Pending => Err(HelperTransportError::Timeout),
                        Outcome::Failed => {
                            Err(HelperTransportError::Transport("connect refused".into()))
                        }
                        _ => unreachable!(),
                    },
                );
            }
            let client = HelperClient::with_config(
                transport.clone(),
                HelperHealth::default(),
                HelperClientConfig::default().without_retries(),
            );
            let params = ShareDeliverySubmissionParams {
                configured_server_urls: &configured,
                now_seconds: SUBMIT_AT,
            };
            let report = if mode == ReportingMode::Plain {
                vote.submit_prepared_shares(&db, &client, params, &never_cancel())
                    .await
                    .unwrap()
            } else {
                let operation = vote
                    .submit_prepared_shares_with_report(
                        &db,
                        &client,
                        params,
                        &never_cancel(),
                        (mode == ReportingMode::Enabled).then(ObservabilityOptions::default),
                    )
                    .await;
                if mode == ReportingMode::Enabled {
                    let diagnostics = operation.observability.unwrap();
                    assert_eq!(diagnostics.outcome, expected);
                    assert_eq!(diagnostics.round_id.as_deref(), Some(ROUND_ID));
                    let stage = diagnostics
                        .records
                        .iter()
                        .find(|record| record.stage.as_ref() == "helper::submit_prepared_shares")
                        .unwrap();
                    assert_eq!(stage.outcome, expected);
                    let identities = diagnostics
                        .records
                        .iter()
                        .filter(|record| record.stage.as_ref() == "helper.http.post_json")
                        .map(|record| {
                            assert_eq!(record.attribution.bundle_index, Some(0));
                            assert_eq!(record.attribution.proposal_id, Some(1));
                            record.attribution.share_index.unwrap()
                        })
                        .collect::<std::collections::BTreeSet<_>>();
                    assert_eq!(identities, (0..16).collect());
                } else {
                    assert!(operation.observability.is_none());
                }
                operation.result.unwrap()
            };
            assert!(report.pending_share_indices.is_empty());
            assert_eq!(report.deliveries.len(), 16);
            assert_eq!(transport.call_count(&post_url), 16);
            let durable = share::list(&db, ROUND_ID)
                .unwrap()
                .into_iter()
                .map(|share| {
                    (
                        share.share_index,
                        share.sent_to_urls,
                        share.ambiguous_urls,
                        share.attempting_urls,
                    )
                })
                .collect::<Vec<_>>();
            if let Some((prior_report, prior_durable)) = &baseline {
                assert_eq!(&report, prior_report);
                assert_eq!(&durable, prior_durable);
            } else {
                baseline = Some((report, durable));
            }
        }
    }
}

/// A persisted atomic batch is submitted once and confirms on its first poll.
pub(super) struct ConfirmingBatchChain {
    confirmation: Mutex<Option<Vec<u8>>>,
    submitted: Mutex<bool>,
    digest: [u8; 32],
}

impl crate::ChainTransport for ConfirmingBatchChain {
    fn chain_get<'a>(
        &'a self,
        _request: crate::ChainHttpRequest,
    ) -> crate::ChainTransportFuture<'a> {
        Box::pin(async {
            Ok(crate::ChainHttpResponse::json(
                200,
                self.confirmation
                    .lock()
                    .unwrap()
                    .take()
                    .expect("exactly one chain poll"),
            ))
        })
    }

    fn chain_post_json<'a>(
        &'a self,
        _request: crate::ChainHttpRequest,
        _json: Vec<u8>,
    ) -> crate::ChainTransportFuture<'a> {
        Box::pin(async {
            let mut submitted = self.submitted.lock().unwrap();
            assert!(!*submitted, "exactly one batch submission");
            *submitted = true;
            Ok(crate::ChainHttpResponse::json(
                200,
                serde_json::to_vec(&serde_json::json!({
                    "tx_hash": hex::encode([0x45; 32]), "code": 0,
                    "batch_digest": hex::encode(self.digest),
                }))
                .unwrap(),
            ))
        })
    }
}

pub(super) struct DeliveryHost;

impl crate::RoundHostSource for DeliveryHost {
    fn host_context(&self) -> crate::RoundHostContext {
        crate::RoundHostContext {
            configured_helper_urls: helpers(1),
            now_seconds: SUBMIT_AT,
            ceremony_start_seconds: Some(0),
            vote_end_time_seconds: Some(VOTE_END),
            vote_tree_node_urls: vec![],
            delegation: None,
            chain_policy: crate::ChainAdvancePolicy::default(),
            max_proof_concurrency: 1,
        }
    }
}

#[tokio::test(start_paused = true)]
async fn atomic_round_delivery_attributes_every_proposal_share_and_retry() {
    let transport = Arc::new(MockTransport::default());
    transport.queue_get(
        &format!("{}/shielded-vote/v1/status", helper(1)),
        json_status("ok"),
    );
    let post_url = format!("{}/shielded-vote/v1/shares", helper(1));
    // One definite transient failure retries while sibling shares run.
    transport.queue_post(&post_url, http_status(429));
    for _ in 0..32 {
        transport.queue_post(&post_url, json_status("queued"));
    }
    let executor =
        atomic_delivery_executor(Arc::new(db_with_round_and_bundle()), transport.clone());
    assert!(matches!(
        executor.plan().unwrap().next_steps[0],
        crate::session::NextStep::AdvanceVoteBatch {
            bundle_index: 0,
            proposal_id: 1,
        }
    ));
    let operation = crate::RoundDriver::new(&executor)
        .with_policy(crate::RoundDrivePolicy {
            max_dispatches: 1,
            ..Default::default()
        })
        .run_with_report(
            &DeliveryHost,
            &crate::ChainSubmissionControl::new(1),
            &crate::NoopRoundDriveReporter::default(),
            Some(ObservabilityOptions::default()),
        )
        .await;
    assert!(
        operation.result.failures.is_empty(),
        "{:?}",
        operation.result.failures
    );
    assert_eq!(operation.result.share_deliveries.len(), 2);
    let diagnostics = operation.observability.unwrap();
    assert_eq!(diagnostics.round_id.as_deref(), Some(ROUND_ID));
    let attempts = diagnostics
        .records
        .iter()
        .filter(|record| record.stage.as_ref() == "helper.http.post_json")
        .collect::<Vec<_>>();
    assert_eq!(attempts.len(), 33);
    let mut identities = std::collections::BTreeSet::new();
    for attempt in &attempts {
        assert_eq!(attempt.attribution.bundle_index, Some(0));
        identities.insert((
            attempt.attribution.proposal_id.unwrap(),
            attempt.attribution.share_index.unwrap(),
        ));
        assert!(attempt.parent_id.is_some());
    }
    assert_eq!(
        identities,
        (1..=2)
            .flat_map(|proposal| (0..16).map(move |share| (proposal, share)))
            .collect()
    );
    let failed = attempts
        .iter()
        .find(|attempt| attempt.http_status == Some(429))
        .unwrap();
    let retried = attempts
        .iter()
        .find(|attempt| attempt.attempt == Some(2))
        .unwrap();
    let retry_wait = diagnostics
        .records
        .iter()
        .find(|record| record.stage.as_ref() == "helper::retry_wait")
        .unwrap();
    assert_eq!(retry_wait.attribution, failed.attribution);
    let step = diagnostics
        .records
        .iter()
        .find(|record| record.stage.as_ref() == "round::advance_step")
        .unwrap();
    assert_eq!(step.attribution.proposal_id, Some(1));
    assert_eq!(step.attribution.share_index, None);
    assert_eq!(failed.endpoint_index, Some(0));
    assert_eq!(retried.endpoint_index, Some(0));
    assert_eq!(retry_wait.endpoint_index, Some(0));
    assert_eq!(failed.attribution, retried.attribution);
    assert_eq!(failed.attempt, Some(1));
    assert_eq!(retried.http_status, Some(200));
    assert_eq!(transport.call_count(&post_url), 33);
    assert_eq!(
        share::list(&executor.database(), ROUND_ID).unwrap().len(),
        32
    );
}

/// Persisted two-member unit driven through the real chain coordinator and
/// round completion path, with scripted transport and no proof generation.
pub(super) fn atomic_delivery_executor(
    db: Arc<VotingDb>,
    transport: Arc<dyn HelperTransport>,
) -> crate::RoundExecutor<ConfirmingBatchChain> {
    db.store_delegation_tx_hash(ROUND_ID, 0, &hex::encode([0x41; 32]))
        .unwrap();
    db.store_van_position(ROUND_ID, 0, 7).unwrap();
    let mut recoveries = [complete_recovery(1), complete_recovery(2)];
    recoveries[0].vc_tree_position = 0;
    recoveries[1].vc_tree_position = 0;
    recoveries[1].van_nullifier = recoveries[0].vote_authority_note_new;
    recoveries[1].vote_authority_note_new = [0x22; 32];
    recoveries[1].vote_commitment = [0x23; 32];
    let actions = recoveries
        .iter()
        .map(
            |recovery| crate::vote_commitment::CastVoteBatchSighashAction {
                r_vpk: &recovery.r_vpk,
                van_nullifier: &recovery.van_nullifier,
                vote_authority_note_new: &recovery.vote_authority_note_new,
                vote_commitment: &recovery.vote_commitment,
                proposal_id: recovery.proposal_id,
            },
        )
        .collect::<Vec<_>>();
    let digest = crate::vote_commitment::cast_vote_batch_sighash(
        ROUND_ID,
        recoveries[0].anchor_height as u64,
        &actions,
    )
    .unwrap();
    for (index, recovery) in recoveries.iter_mut().enumerate() {
        recovery.batch = Some(crate::vote::VoteBatchRecovery {
            delegation_van: None,
            digest,
            index: index as u32,
            size: 2,
        });
        store_recovery(&db, recovery, false);
    }
    let attributes = [
        ("vote_round_id", ROUND_ID.to_string()),
        ("batch_digest", hex::encode(digest)),
        ("batch_size", "2".into()),
        ("final_van_leaf_index", "7".into()),
        ("vote_commitment_leaf_indices", "8,9".into()),
        ("proposal_ids", "1,2".into()),
        (
            "van_nullifiers",
            recoveries
                .iter()
                .map(|recovery| hex::encode(recovery.van_nullifier))
                .collect::<Vec<_>>()
                .join(","),
        ),
    ]
    .into_iter()
    .map(|(key, value)| serde_json::json!({"key": key, "value": value}))
    .collect::<Vec<_>>();
    let chain = ConfirmingBatchChain {
        digest,
        submitted: Mutex::new(false),
        confirmation: Mutex::new(Some(
            serde_json::to_vec(&serde_json::json!({
                "height": "9", "code": 0, "log": "",
                "events": [{"type": "cast_vote_batch", "attributes": attributes}],
            }))
            .unwrap(),
        )),
    };
    crate::RoundExecutor::with_transport(
        db.clone(),
        chain,
        crate::ChainSubmissionClientConfig::for_network(
            crate::Network::Testnet,
            vec!["https://chain.example".into()],
        ),
        HelperClient::new(transport, HelperHealth::default()),
    )
    .unwrap()
    .with_binding(crate::RoundBinding {
        round_id: ROUND_ID.into(),
        network: crate::Network::Testnet,
        proposals: vec![
            crate::ProposalRosterEntry {
                proposal_id: 1,
                num_options: 3,
            },
            crate::ProposalRosterEntry {
                proposal_id: 2,
                num_options: 3,
            },
        ],
        hotkey_secret: None,
    })
    .unwrap()
}

#[tokio::test]
async fn confirmation_diagnostics_distinguish_quorum_persistence_and_reuse() {
    for failure in ["none", "stale", "storage"] {
        let configured = helpers(2);
        let db = Arc::new(db_with_delivery(&configured, &[], 2));
        if failure == "storage" {
            db.conn().execute_batch("CREATE TRIGGER fail_confirmation BEFORE UPDATE OF confirmed ON share_delegations BEGIN SELECT RAISE(FAIL, 'confirmation write failed'); END;").unwrap();
        }
        let transport = Arc::new(MockTransport::default());
        let share_id = share_id_of(&db);
        for endpoint in &configured {
            transport.queue_get(
                &format!("{endpoint}/shielded-vote/v1/share-status/{ROUND_ID}/{share_id}"),
                json_status("confirmed"),
            );
        }
        if failure == "stale" {
            let db = db.clone();
            transport.observe_gets(move |_| {
                db.conn()
                    .execute(
                        "UPDATE share_delegations SET nullifier = ?1",
                        [vec![0xF4; 32]],
                    )
                    .unwrap();
            });
        }
        let client = client_with(transport.clone());
        let params = ShareConfirmationParams {
            round_id: ROUND_ID,
            share: ShareKey {
                bundle_index: 0,
                proposal_id: 1,
                share_index: 0,
            },
            configured_server_urls: &configured,
            now_seconds: SUBMIT_AT,
        };
        let operation = confirm_pending_share_with_report(
            &db,
            &params,
            &client,
            &never_cancel(),
            Some(ObservabilityOptions::default()),
        )
        .await;
        let diagnostics = operation.observability.unwrap();
        let stage = |name: &str| {
            diagnostics
                .records
                .iter()
                .find(|record| record.stage.as_ref() == name)
                .unwrap()
        };
        assert_eq!(
            stage("helper::confirmation_quorum").outcome,
            Outcome::Succeeded
        );
        assert_eq!(stage("helper::share_lock_wait").outcome, Outcome::Succeeded);
        let expected = match failure {
            "none" => Outcome::Succeeded,
            "stale" => Outcome::Pending,
            _ => Outcome::Failed,
        };
        assert_eq!(stage("helper::persist_confirmation").outcome, expected);
        assert_eq!(diagnostics.outcome, expected);
        if failure == "storage" {
            assert!(operation.result.is_err());
        } else {
            assert_eq!(operation.result.unwrap().confirmed, failure == "none");
        }
        assert_eq!(only_share(&db).confirmed, failure == "none");
        if failure == "none" {
            let reused = confirm_pending_share_with_report(
                &db,
                &params,
                &client,
                &never_cancel(),
                Some(ObservabilityOptions::default()),
            )
            .await;
            assert!(reused.result.unwrap().confirmed);
            let records = reused.observability.unwrap().records;
            assert!(records.iter().any(|record| record.stage.as_ref()
                == "helper::confirmation_reused"
                && record.outcome == Outcome::Reused));
            assert!(!records
                .iter()
                .any(|record| record.stage.as_ref() == "helper::confirmation_quorum"));
            assert_eq!(transport.call_count("/share-status/"), 2);
        }
    }
}

#[tokio::test]
async fn status_endpoint_ordinals_survive_health_order_and_keep_pending_semantics() {
    let configured = helpers(5);
    let db = db_with_delivery(&configured, &[], 3);
    let transport = Arc::new(MockTransport::default());
    let share_id = share_id_of(&db);
    for endpoint in &configured {
        transport.queue_get(
            &format!("{endpoint}/shielded-vote/v1/share-status/{ROUND_ID}/{share_id}"),
            json_status("pending"),
        );
    }
    let client = client_with(transport.clone());
    for _ in 0..3 {
        client.health().record_failure(&configured[0], SUBMIT_AT);
    }
    let operation = confirm_pending_share_with_report(
        &db,
        &ShareConfirmationParams {
            round_id: ROUND_ID,
            share: ShareKey {
                bundle_index: 0,
                proposal_id: 1,
                share_index: 0,
            },
            configured_server_urls: &configured,
            now_seconds: SUBMIT_AT,
        },
        &client,
        &never_cancel(),
        Some(ObservabilityOptions::default()),
    )
    .await;
    assert!(!operation.result.unwrap().confirmed);
    let diagnostics = operation.observability.unwrap();
    let status = diagnostics
        .records
        .iter()
        .filter(|record| record.stage.as_ref() == "helper::share_status")
        .collect::<Vec<_>>();
    assert_eq!(status.len(), 5);
    assert_eq!(status.first().unwrap().endpoint_index, Some(1));
    assert_eq!(status.last().unwrap().endpoint_index, Some(0));
    assert!(status
        .iter()
        .all(|record| record.outcome == Outcome::Pending));
    assert_eq!(
        status
            .iter()
            .map(|record| record.endpoint_index.unwrap())
            .collect::<std::collections::BTreeSet<_>>(),
        (0..5).collect()
    );
    assert!(!diagnostics
        .records
        .iter()
        .any(|record| record.stage.as_ref() == "helper::persist_confirmation"));
    assert!(diagnostics
        .records
        .iter()
        .filter(|record| record.stage.as_ref() == "helper.http.get")
        .all(|record| record.endpoint_index.is_some()));
}

#[tokio::test]
async fn cancelled_confirmation_reports_lock_wait_without_polling() {
    let configured = helpers(2);
    let db = db_with_delivery(&configured, &[], 2);
    let scope = share::ShareOperationScope::capture(&db);
    let _guard = lock_share_operation(&scope, ROUND_ID, 0, 1, 0)
        .await
        .unwrap();
    let transport = Arc::new(MockTransport::default());
    let client = client_with(transport.clone());
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let cancel = || cancelled.load(std::sync::atomic::Ordering::SeqCst);
    let params = ShareConfirmationParams {
        round_id: ROUND_ID,
        share: ShareKey {
            bundle_index: 0,
            proposal_id: 1,
            share_index: 0,
        },
        configured_server_urls: &configured,
        now_seconds: SUBMIT_AT,
    };
    let interrupt = async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
    };
    let (operation, ()) = tokio::join!(
        confirm_pending_share_with_report(
            &db,
            &params,
            &client,
            &cancel,
            Some(ObservabilityOptions::default())
        ),
        interrupt,
    );
    assert!(operation.result.unwrap().cancelled);
    let diagnostics = operation.observability.unwrap();
    let lock = diagnostics
        .records
        .iter()
        .find(|record| record.stage.as_ref() == "helper::share_lock_wait")
        .unwrap();
    assert_eq!(lock.outcome, Outcome::Cancelled);
    assert!(lock.elapsed_us >= 10_000);
    assert!(transport.calls().is_empty());
    assert!(!only_share(&db).confirmed);
}
