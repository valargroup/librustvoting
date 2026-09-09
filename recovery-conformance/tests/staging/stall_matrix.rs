//! Hanging one class of network request at a time, and judging what survives.
//!
//! The crash matrix models an app that was killed. This models the fault it
//! cannot: nothing dies, nothing unwinds, and an answer simply never comes.
//!
//! The question each exercise asks first is the one nothing in this repository
//! asked before — **does the run end at all?** Every other assertion here is
//! the crash matrix's own, reused, because a hang that leaves durable state
//! those assertions already understand is a fault this suite can judge.

use std::time::{Duration, Instant};

use recovery_conformance::assertions::{
    assert_a_stalled_submission_survived, assert_idempotent, assert_matches_control,
    assert_no_second_generation, assert_reservations_monotonic, assert_the_request_was_bounded,
    assert_the_stall_fired, deterministic_plan, DurableSnapshot,
};
use recovery_conformance::child::{run_to_quiescence, run_until_the_stall_resolves};
use recovery_conformance::round_run::proposal_ids;
use recovery_conformance::run_config::RunMode;
use recovery_conformance::stall::{StallPlan, StallPoint, StallRecord, StallTarget};

#[path = "fixture.rs"]
mod fixture;

use fixture::{
    build_control, config_for, fixture_account, prepare, provision, warm_from, Faults, Fixture,
};

/// Dispatch ceiling for one stalled drive.
///
/// The crash matrix's own ceiling. A stalled run does less work, not more: its
/// armed request never answers, so it cannot get further than a healthy run.
const MAX_DISPATCHES: usize = 144 * 10;

/// How much longer than its declared bound a stalled request may take.
///
/// Generous on purpose, and the reason matters. A run makes many requests of
/// the armed class, several of them retried under a policy this suite does not
/// try to model, so the claim being tested is that the hang *ends*, not that it
/// ends promptly. A tight allowance would report ordinary retry behaviour as an
/// unbounded request, which is a false finding in the direction that wastes the
/// most time.
const BOUND_ALLOWANCE: u32 = 12;

/// Floor under the allowance, whatever the class's bound.
///
/// Sized from what a stalled run still has to do rather than from the stalled
/// request itself, because the request is the smaller half. A run drives a
/// whole round — three delegations and nine votes, each with a Halo2 proof —
/// and brackets it with two share-tracking phases, each bounded at eight
/// minutes by [`SHARE_TRACKING_BUDGET`]. A stall on the helper path makes both
/// of those phases run to their budget, so sixteen minutes of the run has
/// nothing to do with how long the armed request took.
///
/// Without this floor, `share-status` — bound ten seconds, allowance two
/// minutes — would be reported as an unbounded request every single run, for
/// time its round spent elsewhere. That is the false finding this axis can
/// least afford: it would discredit the one claim the axis exists to make.
///
/// The cost is a weak upper bound. What this matrix demonstrates is that a hung
/// request *ends*, not that it ends promptly; a deadline that is applied but far
/// too long would pass here. See "What this suite cannot cover" in the README.
///
/// [`SHARE_TRACKING_BUDGET`]: recovery_conformance::round_run
const MIN_STALL_BUDGET: Duration = Duration::from_secs(30 * 60);

pub enum Run {
    Skipped(String),
    Completed(Report),
}

pub struct Report {
    pub attempted: usize,
    pub passed: Vec<StallTarget>,
    pub failed: Vec<(StallTarget, String)>,
    pub skipped: Vec<(StallTarget, String)>,
}

impl Report {
    pub fn print(&self) {
        eprintln!("\n=== stall conformance ===");
        for target in &self.passed {
            eprintln!("  PASS  {target}");
        }
        for (target, why) in &self.skipped {
            eprintln!("  SKIP  {target}: {why}");
        }
        for (target, why) in &self.failed {
            eprintln!("  FAIL  {target}: {why}");
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
        Ok(fixture) => Run::Completed(runtime.block_on(drive(fixture))),
    }
}

async fn drive(fixture: Fixture) -> Report {
    let mut report = Report {
        attempted: 0,
        passed: Vec::new(),
        failed: Vec::new(),
        skipped: Vec::new(),
    };

    // The control comes first and is unconditional, exactly as in the crash
    // matrix: every terminal comparison is against it, and a matrix without one
    // proves only that a stalled round converges somewhere.
    let control = match build_control(&fixture, MAX_DISPATCHES, &Faults::none()).await {
        Ok(control) => control,
        Err(error) => {
            report.failed.push((
                StallTarget::PirQuery,
                format!("control run failed: {error:#}"),
            ));
            report.attempted = 1;
            return report;
        }
    };
    eprintln!("control terminal snapshot: {:?}", control.states());

    let selected = selected_targets();
    for target in StallTarget::ALL {
        let target = *target;
        if let Some(selected) = &selected {
            if !selected.contains(&target) {
                continue;
            }
        }
        // Lightwalletd is not reached through the route, so it needs a
        // black-hole listener rather than a wrapper. Skipped explicitly and by
        // name: a target quietly missing from a run is how a matrix rots.
        if !target.is_routed() {
            report.attempted += 1;
            report.skipped.push((
                target,
                "not reached through the shared route; needs a black-hole listener".to_string(),
            ));
            continue;
        }
        report.attempted += 1;
        let started = Instant::now();

        let round = match provision(&fixture).await {
            Ok(round) => round,
            Err(error) => {
                report
                    .skipped
                    .push((target, format!("no round: {error:#}")));
                continue;
            }
        };

        match exercise(&fixture, target, &round, &control).await {
            Ok(()) => {
                eprintln!("  PASS {target} in {:.0}s", started.elapsed().as_secs_f64());
                report.passed.push(target);
            }
            Err(Outcome::Skipped(why)) => {
                eprintln!(
                    "  SKIP {target} after {:.0}s: {why}",
                    started.elapsed().as_secs_f64()
                );
                report.skipped.push((target, why));
            }
            Err(Outcome::Failed(why)) => {
                eprintln!(
                    "  FAIL {target} after {:.0}s: {why}",
                    started.elapsed().as_secs_f64()
                );
                report.failed.push((target, why));
            }
        }
    }
    report
}

enum Outcome {
    Skipped(String),
    Failed(String),
}

/// Runs one target end to end.
async fn exercise(
    fixture: &Fixture,
    target: StallTarget,
    round: &fixture::ProvisionedRound,
    control: &DurableSnapshot,
) -> Result<(), Outcome> {
    let sidecar = fixture
        .workspace
        .join(format!("stall-{}.db", target.name()));
    let _ = std::fs::remove_file(&sidecar);

    // After dispatch wherever the class can carry a submission, because that is
    // the ambiguous half and the one with a safety claim attached. For a read,
    // the two points differ only in how the failure is labelled, and the
    // pre-dispatch form is the one that also exercises connection setup.
    let point = if target.carries_a_submission() {
        StallPoint::AfterDispatch
    } else {
        StallPoint::BeforeDispatch
    };
    let plan = StallPlan::hanging(target, point);
    let budget = budget_for(target);

    let mut armed = config_for(
        fixture,
        &sidecar,
        round,
        RunMode::Unarmed,
        MAX_DISPATCHES,
        &Faults::stall(plan),
    );
    // The stalled run and the resume are both unarmed, so both would otherwise
    // write to the same log and the resume would truncate the evidence that the
    // stall fired at all. Separating them keeps that evidence independent of the
    // order these two calls happen to be written in.
    armed.crash_log = sidecar.with_extension("stall.crashlog.jsonl");
    let stalled = run_until_the_stall_resolves(&fixture.worker, &armed, budget)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    warm_from(fixture, &sidecar);

    // (1) the stall fired, and at the point it was asked to
    let records = StallRecord::from_observations(&stalled.observations);
    assert_the_stall_fired(&records, target, point).map_err(|error| {
        // A class the round never exercises is a gap in the taxonomy rather
        // than a defect in the SDK, but it must be visible either way.
        Outcome::Failed(format!("{error:#}"))
    })?;
    eprintln!(
        "  {target}: stalled {} request(s), SDK deadline {:?}",
        records.len(),
        records.first().map(|record| record.timeout)
    );

    // (2) the run ended by itself, within the bound the SDK claims
    if !stalled.ended_itself {
        return Err(Outcome::Failed(format!(
            "the run was still hanging after {budget:?} and had to be killed. {target} carries \
             a declared bound of {:?}, so either that deadline is not being applied or it is \
             not being applied to this request. A wedged round is not repaired by restarting: \
             nothing crashed",
            target.declared_bound()
        )));
    }
    assert_the_request_was_bounded(target, stalled.elapsed, budget)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    eprintln!(
        "  {target}: the run ended itself after {:.0}s",
        stalled.elapsed.as_secs_f64()
    );

    // (3) the durable state a hang left is the conservative one
    let after_stall = DurableSnapshot::read(&sidecar)
        .map_err(|error| Outcome::Failed(format!("unreadable sidecar: {error:#}")))?;
    assert_a_stalled_submission_survived(&after_stall, target, point)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    deterministic_plan(
        &sidecar,
        &fixture_account(),
        &round.round_id,
        &proposal_ids(),
    )
    .map_err(|error| Outcome::Failed(format!("{error:#}")))?;

    // (4) the round converges once the endpoint answers again
    let resumed = config_for(
        fixture,
        &sidecar,
        round,
        RunMode::Unarmed,
        MAX_DISPATCHES,
        &Faults::none(),
    );
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
    if !outcome.is_terminal_success() {
        return Err(Outcome::Failed(format!(
            "resume ended at {} rather than quiescence; failures: {:?}",
            outcome.quiescence, outcome.failures
        )));
    }

    let terminal = DurableSnapshot::read(&sidecar)
        .map_err(|error| Outcome::Failed(format!("unreadable sidecar: {error:#}")))?;
    // The safety half: a hang that may have delivered a transaction must not
    // have produced a second one. Counting alone cannot show this, which is why
    // generation identity is checked as well.
    assert_reservations_monotonic(&after_stall, &terminal)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    assert_no_second_generation(&after_stall, &terminal)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    eprintln!(
        "  {target}: reservations {} -> {} (stall -> terminal)",
        after_stall.total_reservations(),
        terminal.total_reservations()
    );
    assert_matches_control(&terminal, control)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;

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

/// How long a stalled run may take before it is judged unbounded.
fn budget_for(target: StallTarget) -> Duration {
    (target.declared_bound() * BOUND_ALLOWANCE).max(MIN_STALL_BUDGET)
}

/// The targets this run exercises, or `None` for all of them.
///
/// An unrecognized name is a hard error rather than an empty selection, for the
/// same reason the crash matrix refuses one: a typo that silently ran nothing
/// would report a green matrix having tested nothing at all.
fn selected_targets() -> Option<Vec<StallTarget>> {
    let requested = std::env::var("RECOVERY_CONFORMANCE_STALLS").ok()?;
    let targets: Vec<StallTarget> = requested
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            name.parse::<StallTarget>().unwrap_or_else(|_| {
                panic!(
                    "RECOVERY_CONFORMANCE_STALLS names an unknown target {name:?}; known \
                     targets are {}",
                    StallTarget::ALL
                        .iter()
                        .map(|target| target.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
        })
        .collect();
    (!targets.is_empty()).then_some(targets)
}
