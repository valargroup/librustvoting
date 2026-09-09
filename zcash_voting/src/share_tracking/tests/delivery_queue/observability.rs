use super::{fixtures::*, *};
use crate::{ObservabilityOptions, ObservationOutcome as Outcome};
use tokio::sync::Semaphore;

#[tokio::test]
async fn queue_residence_precedes_admission_and_active_delivery_includes_journaling() {
    let fixture = Fixture::new(4);
    let gate = Arc::new(Semaphore::new(0));
    let transport = ScriptedTransport::new({
        let gate = gate.clone();
        move |_| ReplyPlan {
            gate: Some(gate.clone()),
            ..Default::default()
        }
    });
    let invocation =
        crate::ObservationScope::new(Some(ObservabilityOptions::default())).invocation();
    let client =
        HelperClient::new(transport.clone(), HelperHealth::default()).observing(invocation.scope());
    let release = async {
        transport.wait_for(16).await;
        gate.add_permits(4 * SHARE_COUNT);
    };
    let mut on_report = |_: &crate::vote::CommittedVote, _: &ShareBatchDeliveryReport| {};
    let (reports, ()) = tokio::join!(
        crate::vote::submit_confirmed_vote_shares(
            &fixture.votes,
            &fixture.db,
            &client,
            ShareDeliverySubmissionParams {
                configured_server_urls: &fixture.configured,
                now_seconds: SUBMIT_AT
            },
            &uncancelled,
            &mut on_report,
        ),
        release,
    );
    assert_complete(reports.into_iter().map(|vote| vote.delivery).collect(), 4);
    let diagnostics = invocation
        .complete("delivery", Outcome::Succeeded, ())
        .observability
        .unwrap();
    assert_eq!(diagnostics.records_dropped, 0);
    assert_eq!(diagnostics.active_stages_dropped, 0);
    for proposal in 1..=4 {
        for share_index in 0..SHARE_COUNT as u32 {
            let records = diagnostics
                .records
                .iter()
                .filter(|record| {
                    record.attribution.proposal_id == Some(proposal)
                        && record.attribution.share_index == Some(share_index)
                })
                .collect::<Vec<_>>();
            let record = |stage: &str| {
                *records
                    .iter()
                    .find(|record| record.stage.as_ref() == stage)
                    .unwrap()
            };
            let queued = record("helper::delivery_queue_wait");
            let active = record("helper::active_delivery");
            let post = record("helper::post_share");
            let persisted = record("helper::persist_acceptance");
            assert!(queued.started_after_us + queued.elapsed_us <= active.started_after_us);
            assert!(post.started_after_us + post.elapsed_us <= persisted.started_after_us);
            assert!(
                persisted.started_after_us + persisted.elapsed_us
                    <= active.started_after_us + active.elapsed_us
            );
            assert_eq!(post.outcome, Outcome::Pending);
            assert_eq!(persisted.outcome, Outcome::Succeeded);
            assert_eq!(post.endpoint_index, Some(0));
            assert_eq!(record("helper.http.post_json").endpoint_index, Some(0));
            assert_eq!(record("helper::post_capacity_wait").endpoint_index, Some(0));
        }
    }
    let first_finished = diagnostics
        .records
        .iter()
        .filter(|record| record.stage.as_ref() == "helper::active_delivery")
        .map(|record| record.started_after_us + record.elapsed_us)
        .min()
        .unwrap();
    let last_proposal = diagnostics.records.iter().filter(|record| {
        record.stage.as_ref() == "helper::delivery_queue_wait"
            && record.attribution.proposal_id == Some(4)
    });
    for queued in last_proposal {
        assert!(queued.started_after_us < first_finished);
        assert!(queued.started_after_us + queued.elapsed_us >= first_finished);
    }
}
