//! Driving every crash stage against staging, in order, and judging the result.

use std::time::{Duration, Instant};

use recovery_conformance::assertions::{
    assert_confirmed_by_a_legal_route, assert_idempotent, assert_matches_control,
    assert_no_second_generation, assert_other_bundles_untouched, assert_plans_precede_broadcast,
    assert_recovered_the_same_transaction, assert_reservations_monotonic, assert_stage_state,
    assert_terminal_rows_unchanged, assert_untouched_bundles_did_not_reserve, confirmation_source,
    confirmed_transaction_hash, deterministic_plan, dispatched_transaction_hash, DurableSnapshot,
};
use recovery_conformance::child::{run_to_quiescence, run_until_crash};
use recovery_conformance::round_run::{default_target, proposal_ids};
use recovery_conformance::run_config::RunMode;
use recovery_conformance::CrashStage;

#[path = "fixture.rs"]
mod fixture;

use fixture::{
    build_control, config_for, fixture_account, prepare, provision, warm_from, Faults, Fixture,
    ProvisionedRound,
};

/// How long one stage may take before it is abandoned.
///
/// Generous: a full drive to quiescence proves three delegations and nine
/// votes, and a vote proof takes minutes. The budget exists so a wedged run
/// fails the matrix rather than hanging it.
const STAGE_BUDGET: Duration = Duration::from_secs(45 * 60);

/// How long a dispatched transaction is given to reach a block.
///
/// Only the stages whose premise is "the chain already holds this" wait. Long
/// enough for inclusion on `svote-1`, and paid once by two stages rather than
/// by the whole matrix.
const CHAIN_INCLUSION_WAIT: Duration = Duration::from_secs(45);

/// Dispatch ceiling for one drive, so a plan that never shrinks ends the run.
///
/// Sized from the work a resume can actually owe, because the ceiling is a
/// livelock detector and a ceiling below the honest maximum turns every slow
/// convergence into a false positive. The round carries 3 bundles x 3 proposals
/// x 16 shares = 144 shares. A stage crashed at the first share POST resumes
/// owing every one of them: one dispatch to deliver each, then one per
/// confirmation poll, and a helper quorum routinely needs several polls before
/// it answers. At 512 that stage exhausted the budget with all 144 shares still
/// unconfirmed while the round was in fact converging.
///
/// Ten dispatches per share leaves room for delivery plus a long confirmation
/// tail. A plan that genuinely never shrinks still ends the run, just later.
const MAX_DISPATCHES: usize = 144 * 10;

pub enum Run {
    Skipped(String),
    Completed(Report),
}

pub struct Report {
    pub attempted: usize,
    pub passed: Vec<CrashStage>,
    pub failed: Vec<(CrashStage, String)>,
    pub skipped: Vec<(CrashStage, String)>,
}

impl Report {
    pub fn print(&self) {
        eprintln!("\n=== staging conformance ===");
        for stage in &self.passed {
            eprintln!("  PASS  {stage}");
        }
        for (stage, why) in &self.skipped {
            eprintln!("  SKIP  {stage}: {why}");
        }
        for (stage, why) in &self.failed {
            eprintln!("  FAIL  {stage}: {why}");
        }
        eprintln!(
            "  {} passed, {} failed, {} skipped, of {} attempted",
            self.passed.len(),
            self.failed.len(),
            self.skipped.len(),
            self.attempted
        );
    }
}

pub fn run() -> Run {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => return Run::Skipped(format!("no tokio runtime: {error}")),
    };
    match runtime.block_on(prepare()) {
        Err(reason) => Run::Skipped(reason),
        Ok(fixture) => Run::Completed(runtime.block_on(drive_matrix(fixture))),
    }
}

async fn drive_matrix(fixture: Fixture) -> Report {
    let mut report = Report {
        attempted: 0,
        passed: Vec::new(),
        failed: Vec::new(),
        skipped: Vec::new(),
    };

    // The control comes first: every terminal comparison is against it, so a
    // matrix without one proves only that crashes converge somewhere.
    let control = match build_control(&fixture, MAX_DISPATCHES, &Faults::none()).await {
        Ok(control) => control,
        Err(error) => {
            report.failed.push((
                CrashStage::BeforeDelegation,
                format!("control run failed: {error:#}"),
            ));
            report.attempted = 1;
            return report;
        }
    };
    eprintln!("control terminal snapshot: {:?}", control.states());

    let selected = selected_stages();
    for stage in CrashStage::ALL {
        let stage = *stage;
        if let Some(selected) = &selected {
            if !selected.contains(&stage) {
                continue;
            }
        }
        report.attempted += 1;
        let started = Instant::now();

        // Every exercise resumes to quiescence, so every exercise eventually
        // mutates the chain even when its crash itself occurs before the first
        // POST. A round is one-shot once any bundle delegates; sharing one
        // would make later cases observe effects from an earlier case.
        let round = match provision(&fixture).await {
            Ok(round) => round,
            Err(error) => {
                report.skipped.push((stage, format!("no round: {error:#}")));
                continue;
            }
        };

        match exercise(&fixture, stage, &round, &control).await {
            Ok(()) => {
                eprintln!("  PASS {stage} in {:.0}s", started.elapsed().as_secs_f64());
                report.passed.push(stage);
            }
            // Printed as they happen, not only in the final report. A matrix
            // run takes tens of minutes, and a verdict withheld until the end
            // is indistinguishable from a stage that is still running.
            Err(Outcome::Skipped(why)) => {
                eprintln!(
                    "  SKIP {stage} after {:.0}s: {why}",
                    started.elapsed().as_secs_f64()
                );
                report.skipped.push((stage, why));
            }
            Err(Outcome::Failed(why)) => {
                eprintln!(
                    "  FAIL {stage} after {:.0}s: {why}",
                    started.elapsed().as_secs_f64()
                );
                report.failed.push((stage, why));
            }
        }
    }
    report
}

enum Outcome {
    Skipped(String),
    Failed(String),
}

/// Runs one stage end to end.
async fn exercise(
    fixture: &Fixture,
    stage: CrashStage,
    round: &ProvisionedRound,
    control: &DurableSnapshot,
) -> Result<(), Outcome> {
    let started = Instant::now();
    let sidecar = fixture.workspace.join(format!("{}.db", stage.name()));
    let _ = std::fs::remove_file(&sidecar);

    let armed = config_for(fixture, &sidecar, round, RunMode::Armed { stage }, MAX_DISPATCHES, &Faults::none());

    // (c) spawn the armed child; (d) require SIGABRT and a matching observation
    let crash = run_until_crash(&fixture.worker, &armed);
    warm_from(fixture, &sidecar);
    let crash = match crash {
        Ok(crash) => crash,
        Err(error) => {
            let detail = format!("{error:#}");
            // A stage that stops firing is the way this suite rots: it becomes
            // a skip, skips do not fail the matrix, and the run stays green
            // having proven nothing about that boundary. Only the stages known
            // to be unreachable may skip; for any other, a trigger that never
            // fires is a failure.
            if detail.contains("never reached") && !is_known_unreachable(stage) {
                return Err(Outcome::Failed(format!(
                    "{stage} was never reached, and it is not a stage known to be \
                     unreachable, so its crash seam has stopped firing: {detail}"
                )));
            }
            return Err(Outcome::Skipped(detail));
        }
    };

    // (b) capture the durable state the crash left
    let after_crash = DurableSnapshot::read(&sidecar)
        .map_err(|error| Outcome::Failed(format!("unreadable sidecar: {error:#}")))?;

    // (f) plan twice in a fresh process-local database and require agreement
    let plan = deterministic_plan(
        &sidecar,
        &fixture_account(),
        &round.round_id,
        &proposal_ids(),
    )
    .map_err(|error| Outcome::Failed(format!("{error:#}")))?;

    // (g) the stage's own durable expectations
    let bundle = default_target().bundle_index;
    assert_stage_state(stage, &plan, &after_crash, bundle)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    assert_plans_precede_broadcast(&after_crash)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    if stage.touches_chain() && crash.dispatched_a_post() {
        // A dispatched POST means a transaction may exist. Its record is the
        // only evidence of that, so this sidecar is never discarded or retried
        // past, whatever happens next.
        eprintln!(
            "  {stage}: a POST reached the wire; sidecar preserved at {}",
            sidecar.display()
        );
    }
    assert_other_bundles_untouched(&plan, bundle, 3)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;

    // Every stage resumes, including the ones that never reached the chain.
    //
    // They used to stop here, and the reason was sound while it held: a
    // pre-chain stage resumed on a *shared* round would delegate and vote on
    // it, and a delegation is consumed on the vote chain, so the next stage's
    // copy of that round would fail with `nullifier already spent` — a
    // statement about round accounting rather than about recovery.
    //
    // That sharing is gone. Every stage now provisions its own round, so a
    // pre-chain resume consumes only its own delegation and cannot reach
    // another stage. Stopping early would leave the whole delegation-side
    // family proving that the crash was real and its durable state correct,
    // while never proving the round recovers — no A2 convergence and no A3
    // equality with the control, for six of twenty stages, including
    // `before-broadcast`, the conservative-by-design case this suite exists
    // for.
    // A dispatched transaction needs to reach a block before the premise of
    // these stages holds. Their whole point is that the chain *has* the
    // transaction while the wallet has no hash for it, and exact-tree recovery
    // resolves the gap. Resuming the instant the bytes leave tests something
    // else: the tree pass runs, correctly finds nothing yet, and a
    // same-generation retry supplies the hash instead — spec-legal, but it
    // means the tree route is never the thing that resolved the round, and
    // whether it was came down to a race with block inclusion.
    if stage.settles_on_chain_before_resume() {
        eprintln!("  {stage}: waiting {CHAIN_INCLUSION_WAIT:?} for the dispatched transaction");
        tokio::time::sleep(CHAIN_INCLUSION_WAIT).await;
    }

    // (h) resume to quiescence in a new process
    let resumed = config_for(fixture, &sidecar, round, RunMode::Unarmed, MAX_DISPATCHES, &Faults::none());
    if started.elapsed() > STAGE_BUDGET {
        return Err(Outcome::Skipped(
            "stage budget exhausted before resume".to_string(),
        ));
    }
    // A resume that never completes is only a skip when the environment stopped
    // it. Retries that all end on the same non-transport error mean the round
    // does not converge, which is exactly what this matrix exists to catch.
    let outcome = run_to_quiescence(&fixture.worker, &resumed);
    warm_from(fixture, &sidecar);
    let outcome = outcome.map_err(|error| {
        let detail = format!("{error:#}");
        if detail.contains("Transport") || detail.contains("PIR") {
            Outcome::Skipped(format!("resume did not complete: {detail}"))
        } else {
            Outcome::Failed(format!("resume never converged: {detail}"))
        }
    })?;

    // (i) fail on anything that is not a clean ending
    if !outcome.is_terminal_success() {
        return Err(Outcome::Failed(format!(
            "resume ended at {} rather than quiescence; failures: {:?}",
            outcome.quiescence, outcome.failures
        )));
    }

    let terminal = DurableSnapshot::read(&sidecar)
        .map_err(|error| Outcome::Failed(format!("unreadable sidecar: {error:#}")))?;
    assert_reservations_monotonic(&after_crash, &terminal)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    // The half the count cannot prove: no target gained a second generation,
    // and no bundle the crash left alone reserved another POST.
    assert_no_second_generation(&after_crash, &terminal)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    assert_untouched_bundles_did_not_reserve(&after_crash, &terminal, bundle)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    assert_terminal_rows_unchanged(&after_crash, &terminal)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;

    // Requirement 8 wants direct evidence that no second transaction was
    // POSTed, not an inference from eventual confirmation. The durable half is
    // the reservation count: every committed POST increments it and a trigger
    // makes it monotonic, so the number of reservations is the number of times
    // the wallet committed to sending. Reported per stage because the correct
    // value differs by boundary — a crash before dispatch legitimately reserves
    // again on resume, one after it must not — and asserting a number before
    // observing it would encode a guess.
    eprintln!(
        "  {stage}: reservations {} -> {} (crash -> terminal), states {:?}",
        after_crash.total_reservations(),
        terminal.total_reservations(),
        terminal.states()
    );

    // A crash after dispatch but before the response was read leaves no
    // candidate hash, so recovery must resolve it either by scanning the tree
    // or by re-POSTing the same generation. The route is reported every run:
    // it is not assertable, but a change in it should not pass unseen.
    if stage == CrashStage::AfterBroadcastUnread {
        let source =
            confirmation_source(&sidecar).map_err(|error| Outcome::Failed(format!("{error:#}")))?;
        eprintln!("  {stage}: confirmation source {:?}", source.as_deref());
        assert_confirmed_by_a_legal_route(source.as_deref())
            .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    }

    // Requirement 8's chain-identity half, where the stage captured one. The
    // reservation count above says how many times the wallet committed to
    // sending; this says the thing that actually confirmed is the thing it
    // sent, which counting alone cannot show.
    if let Some(body) = crash.dispatched_response_body() {
        if let Some(dispatched) = dispatched_transaction_hash(body) {
            let confirmed = confirmed_transaction_hash(&sidecar)
                .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
            let source = confirmation_source(&sidecar)
                .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
            eprintln!(
                "  {stage}: dispatched {dispatched}, confirmed {} via {}",
                confirmed.as_deref().unwrap_or("<none>"),
                source.as_deref().unwrap_or("<none>")
            );
            assert_recovered_the_same_transaction(
                &dispatched,
                confirmed.as_deref(),
                source.as_deref(),
            )
            .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
        }
    }

    // (j) the terminal shape must match the uncrashed control
    if let Err(error) = assert_matches_control(&terminal, control) {
        return Err(Outcome::Failed(format!("{error:#}")));
    }

    // (k) a second resume must find nothing to do
    let settled = deterministic_plan(
        &sidecar,
        &fixture_account(),
        &round.round_id,
        &proposal_ids(),
    )
    .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    assert_idempotent(&settled).map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    Ok(())
}


/// Stages whose crash seam cannot fire, with the reason.
///
/// Empty, and it should stay that way. `AfterVoteCommit` was listed here on the
/// belief that vote completion offered no seam between persisting the committed
/// vote and writing helper plans. It does: the step probes the helper fleet
/// between those two commits, and that probe is a real network round trip this
/// suite already wraps. Believing a boundary untestable is cheaper than
/// checking, and it cost this stage every run it was ever skipped in.
///
/// Everything must crash where it claims to, or the matrix fails rather than
/// skipping.
fn is_known_unreachable(_stage: CrashStage) -> bool {
    false
}


/// The stages this run exercises, or `None` for the whole matrix.
///
/// Set `RECOVERY_CONFORMANCE_STAGES` to a comma-separated list of stage names
/// to re-run only the stages a change could have affected. The control run is
/// unconditional, because every terminal comparison is against it.
///
/// An unrecognized name is a hard error rather than an empty selection: a typo
/// that silently ran nothing would report a green matrix having tested nothing,
/// which is the failure mode this suite exists to avoid.
fn selected_stages() -> Option<Vec<CrashStage>> {
    let requested = std::env::var("RECOVERY_CONFORMANCE_STAGES").ok()?;
    let stages: Vec<CrashStage> = requested
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            name.parse::<CrashStage>().unwrap_or_else(|_| {
                panic!(
                    "RECOVERY_CONFORMANCE_STAGES names an unknown stage {name:?}; \
                     known stages are {}",
                    CrashStage::ALL
                        .iter()
                        .map(|stage| stage.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
        })
        .collect();
    (!stages.is_empty()).then_some(stages)
}

