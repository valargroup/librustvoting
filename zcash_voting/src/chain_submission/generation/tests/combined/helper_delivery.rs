//! Combined-envelope reconciliation reaches the same cross-proposal queue.

use super::*;
use crate::{
    backend::pasta_curves::{
        group::{ff::PrimeField, Group, GroupEncoding},
        pallas,
    },
    helper::{
        client::HelperClient,
        health::HelperHealth,
        transport::{HelperFuture, HelperResponse, HelperTransport},
    },
    RoundHostContext, RoundHostSource,
};
use std::time::Duration;

struct Helpers {
    db: Arc<VotingDb>,
    completed: Mutex<Vec<(u32, u32)>>,
}

impl HelperTransport for Helpers {
    fn get<'a>(&'a self, _url: &'a str, _timeout: Duration) -> HelperFuture<'a> {
        Box::pin(async { Ok(HelperResponse::json(200, br#"{"status":"ok"}"#.to_vec())) })
    }

    fn post_json<'a>(
        &'a self,
        _url: &'a str,
        json: Vec<u8>,
        _timeout: Duration,
    ) -> HelperFuture<'a> {
        Box::pin(async move {
            let wire: crate::wire::VoteShareWire = serde_json::from_slice(&json).unwrap();
            assert_eq!(
                self.db.delegation_phase(ROUND, 0).unwrap(),
                crate::phases::DelegationPhase::Confirmed
            );
            let confirmed: i64 = self
                .db
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM votes WHERE vc_tree_position IS NOT NULL",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(confirmed, 2);
            assert_eq!(wire.vc_tree_position, 7 + u64::from(wire.proposal_id));
            if wire.proposal_id == 1 && wire.share_index == 0 {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            self.completed
                .lock()
                .unwrap()
                .push((wire.proposal_id, wire.share_index));
            Ok(HelperResponse::json(
                200,
                br#"{"status":"queued"}"#.to_vec(),
            ))
        })
    }
}

struct Host;
impl RoundHostSource for Host {
    fn host_context(&self) -> RoundHostContext {
        RoundHostContext {
            configured_helper_urls: vec!["https://helper.example".into()],
            now_seconds: 1_000,
            ceremony_start_seconds: Some(0),
            vote_end_time_seconds: Some(1_000_000),
            vote_tree_node_urls: vec![],
            delegation: None,
            chain_policy: ChainAdvancePolicy::default(),
            max_proof_concurrency: 1,
        }
    }
}

#[tokio::test(start_paused = true)]
async fn combined_reconciliation_delivers_later_proposals_while_the_first_is_unfinished() {
    let (db, request) = fixture(2);
    // The chain fixture normally omits valid helper payloads. Complete its
    // recovery material before any dispatch, preserving the signed actions.
    for proposal in 1..=2 {
        let (Some(json), _) = db
            .get_commitment_bundle_recovery_fields(ROUND, 0, proposal)
            .unwrap()
            .unwrap()
        else {
            panic!("fixture has recovery material");
        };
        let mut recovery = crate::vote::parse_recovery(&json).unwrap();
        let count = crate::share_policy::VOTE_COMMITMENT_SHARE_COUNT;
        recovery.encrypted_shares = (0..count)
            .map(|index| crate::EncryptedShare {
                c1: (pallas::Point::generator() * pallas::Scalar::from(index as u64 * 2 + 1))
                    .to_bytes()
                    .to_vec(),
                c2: (pallas::Point::generator() * pallas::Scalar::from(index as u64 * 2 + 2))
                    .to_bytes()
                    .to_vec(),
                share_index: index as u32,
                plaintext_value: index as u64 + 1,
                randomness: vec![index as u8 + 1; 32],
            })
            .collect();
        recovery.share_blinds = (0..count)
            .map(|index| pallas::Base::from(index as u64 + 1).to_repr())
            .collect();
        recovery.share_comms = (0..count)
            .map(|index| pallas::Base::from(index as u64 + 10).to_repr())
            .collect();
        crate::vote::insert_recovery_fixture(&db, &recovery).unwrap();
        db.set_ballot_intent(ROUND, proposal, crate::session::Decision::Choice(2), 3)
            .unwrap();
    }
    let chain = Arc::new(Transport {
        request,
        calls: Mutex::new(Vec::new()),
    });
    let helpers = Arc::new(Helpers {
        db: db.clone(),
        completed: Mutex::new(Vec::new()),
    });
    let executor = crate::RoundExecutor::with_transport(
        db.clone(),
        chain.clone(),
        ChainSubmissionClientConfig::for_network(
            crate::Network::Testnet,
            vec!["https://vote.example".into()],
        ),
        HelperClient::new(helpers.clone(), HelperHealth::default()),
    )
    .unwrap()
    .with_binding(crate::RoundBinding {
        round_id: ROUND.into(),
        network: crate::Network::Testnet,
        hotkey_secret: None,
        proposals: (1..=2)
            .map(|proposal_id| crate::ProposalRosterEntry {
                proposal_id,
                num_options: 3,
            })
            .collect(),
    })
    .unwrap();
    let report = crate::RoundDriver::new(&executor)
        .with_policy(crate::RoundDrivePolicy {
            max_dispatches: 1,
            ..Default::default()
        })
        .run(
            &Host,
            &ChainSubmissionControl::new(1),
            &crate::NoopRoundDriveReporter::default(),
        )
        .await;
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    assert_eq!(report.share_deliveries.len(), 2);
    assert_eq!(report.chain_outcomes.len(), 1);
    let completed = helpers.completed.lock().unwrap();
    assert_eq!(
        completed.len(),
        2 * crate::share_policy::VOTE_COMMITMENT_SHARE_COUNT
    );
    assert_eq!(completed.last(), Some(&(1, 0)));
    assert_eq!(
        chain
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|url| url.ends_with("/delegate-and-cast-vote-batch"))
            .count(),
        1
    );
}
