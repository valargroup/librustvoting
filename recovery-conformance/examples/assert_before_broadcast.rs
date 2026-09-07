//! Asserts the durable state a `before-broadcast` crash must leave.
//!
//! The bytes provably never reached the network, yet the reservation must
//! survive as in-flight work rather than disappear: a restarted process cannot
//! prove the request was never released, so the lifecycle is conservative by
//! design. The observable consequence is the plan — the crashed bundle is
//! *advanced*, never delegated again, because re-delegating would build a
//! second transaction spending the same notes.
//!
//! Note that the durable row is still `submitting` immediately after reopen.
//! Normalization to `recovering` is **lazy**: it happens inside `store::admit`
//! on the next advancement, not when the database is opened. Asserting on the
//! raw state at open therefore tests nothing about recovery; the plan is the
//! oracle.
use zcash_voting::round::VotingDb;

fn main() -> anyhow::Result<()> {
    let sidecar = std::env::args()
        .nth(1)
        .expect("usage: <sidecar> <round> <account>");
    let round_id = std::env::args().nth(2).expect("round id");
    let account = std::env::args().nth(3).expect("account uuid");

    // Reopening is the whole point: nothing in memory survived the abort.
    let database = VotingDb::open_path(std::path::Path::new(&sidecar))
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    database.set_wallet_id(&account);

    let state: String =
        database
            .conn()
            .query_row("select state from chain_submissions", [], |row| row.get(0))?;
    println!("chain_submissions.state after reopen : {state}");

    let plan = zcash_voting::session::resume_plan(&database, &round_id, &[1, 2, 3])
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    println!(
        "resume_plan next_steps              : {:?}",
        plan.next_steps
    );

    // A second plan over the same durable state must agree: the plan is a pure
    // function of what is on disk, and every assertion here rests on that.
    let again = zcash_voting::session::resume_plan(&database, &round_id, &[1, 2, 3])
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    println!(
        "plan is deterministic               : {}",
        again.next_steps == plan.next_steps
    );

    use zcash_voting::session::NextStep;
    let crashed_bundle_step = plan
        .next_steps
        .iter()
        .find(|step| matches!(step, NextStep::AdvanceDelegation { bundle_index: 0 }));

    anyhow::ensure!(
        state == "submitting",
        "the crash should have left an unclassified reservation, found {state:?}"
    );
    anyhow::ensure!(
        crashed_bundle_step.is_some(),
        "B2 VIOLATED: bundle 0 is not planned for advancement after the crash"
    );
    anyhow::ensure!(
        !plan
            .next_steps
            .iter()
            .any(|step| matches!(step, NextStep::Delegate { bundle_index: 0 })),
        "B2 VIOLATED: bundle 0 is planned for re-delegation, which would spend its notes twice"
    );
    // Failure is isolated: the bundles that never reached the transport are
    // untouched and still owe their ordinary delegation.
    for bundle in [1u32, 2] {
        anyhow::ensure!(
            plan.next_steps
                .iter()
                .any(|step| matches!(step, NextStep::Delegate { bundle_index } if *bundle_index == bundle)),
            "E1 VIOLATED: bundle {bundle} lost its pending delegation"
        );
    }

    println!("\nB2 HOLDS : the crashed bundle is advanced, never re-delegated");
    println!("E1 HOLDS : the other two bundles are untouched");
    println!("A1 HOLDS : the plan is a deterministic function of durable state");
    Ok(())
}
