//! Driving a round through a fleet of ten helpers whose reachability changes.
//!
//! What this adds to the crash matrix is not a new kind of fault but a fleet
//! large enough for the SDK's placement rules to have anything to decide.
//! Against one helper the target count is one, the per-helper quota is the
//! whole commitment, and every rule about splitting shares, repairing a partial
//! deficit, and resuming against a plan whose targets are now unreachable is
//! unreachable code. Against ten, each of those becomes a statement a run can
//! be wrong about.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use recovery_conformance::assertions::{
    assert_acceptances_never_downgraded, assert_every_share_is_confirmed,
    assert_every_unanswered_helper_was_journalled, assert_idempotent,
    assert_no_contact_outside_the_fleet, assert_no_premature_resend_to_an_accepted_helper,
    assert_placement_stays_within_the_fleet, assert_reservations_monotonic, deterministic_plan,
    placement_spread, DurableSnapshot,
};
use recovery_conformance::child::{run_to_quiescence, run_until_crash, CrashLog};
use recovery_conformance::helper_fleet::{FleetScenario, HelperContacts};
use recovery_conformance::round_run::{helper_backend, proposal_ids};
use recovery_conformance::run_config::RunMode;
use zcash_voting::share_policy::share_submission_target_count;

#[path = "fixture.rs"]
mod fixture;

use fixture::{
    build_control, config_for, fixture_account, prepare, provision, warm_from, Faults, Fixture,
    ProvisionedRound,
};

/// Dispatch ceiling for one fleet drive.
///
/// Five times the crash matrix's, and for a concrete reason rather than for
/// safety. That ceiling is sized from the work a resume can actually owe — 144
/// shares, ten dispatches each — and a target count of five means each of those
/// shares owes five acceptances rather than one. Inheriting the smaller number
/// would turn ordinary convergence into a livelock report, which is precisely
/// the false positive the original comment warns about.
const MAX_DISPATCHES: usize = 144 * 5 * 10;

pub enum Run {
    Skipped(String),
    Completed(Report),
}

pub struct Report {
    pub attempted: usize,
    pub passed: Vec<FleetScenario>,
    pub failed: Vec<(FleetScenario, String)>,
    pub skipped: Vec<(FleetScenario, String)>,
}

impl Report {
    pub fn print(&self) {
        eprintln!("\n=== helper fleet conformance ===");
        for scenario in &self.passed {
            eprintln!("  PASS  {scenario}");
        }
        for (scenario, why) in &self.skipped {
            eprintln!("  SKIP  {scenario}: {why}");
        }
        for (scenario, why) in &self.failed {
            eprintln!("  FAIL  {scenario}: {why}");
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
    let backend = helper_backend(&fixture.deployment);

    // The control is built against this matrix's *own* fleet. Comparing a
    // ten-helper round to the crash matrix's one-helper control would compare
    // two different rounds and report the difference as a finding.
    let control_faults = Faults::fleet(FleetScenario::FullFleetThenCrash.second_fleet(&backend));
    let control = match build_control(&fixture, MAX_DISPATCHES, &control_faults).await {
        Ok(control) => control,
        Err(error) => {
            report.failed.push((
                FleetScenario::FullFleetThenCrash,
                format!("control run failed: {error:#}"),
            ));
            report.attempted = 1;
            return report;
        }
    };
    eprintln!(
        "control: {} share(s), placement {:?}",
        control.deliveries.len(),
        control
            .deliveries
            .first()
            .map(|delivery| delivery.sent.len())
    );

    let selected = selected_scenarios();
    for scenario in FleetScenario::ALL {
        let scenario = *scenario;
        if let Some(selected) = &selected {
            if !selected.contains(&scenario) {
                continue;
            }
        }
        report.attempted += 1;
        let started = Instant::now();

        let round = match provision(&fixture).await {
            Ok(round) => round,
            Err(error) => {
                report
                    .skipped
                    .push((scenario, format!("no round: {error:#}")));
                continue;
            }
        };

        match exercise(&fixture, scenario, &backend, &round, &control).await {
            Ok(()) => {
                eprintln!(
                    "  PASS {scenario} in {:.0}s",
                    started.elapsed().as_secs_f64()
                );
                report.passed.push(scenario);
            }
            Err(Outcome::Skipped(why)) => {
                eprintln!(
                    "  SKIP {scenario} after {:.0}s: {why}",
                    started.elapsed().as_secs_f64()
                );
                report.skipped.push((scenario, why));
            }
            Err(Outcome::Failed(why)) => {
                eprintln!(
                    "  FAIL {scenario} after {:.0}s: {why}",
                    started.elapsed().as_secs_f64()
                );
                report.failed.push((scenario, why));
            }
        }
    }
    report
}

enum Outcome {
    Skipped(String),
    Failed(String),
}

/// Runs one scenario end to end.
async fn exercise(
    fixture: &Fixture,
    scenario: FleetScenario,
    backend: &str,
    round: &ProvisionedRound,
    control: &DurableSnapshot,
) -> Result<(), Outcome> {
    let sidecar = fixture
        .workspace
        .join(format!("fleet-{}.db", scenario.name()));
    let _ = std::fs::remove_file(&sidecar);

    let first = scenario.first_fleet(backend);
    let second = scenario.second_fleet(backend);
    let first_urls = first.configured_urls();
    let second_urls = second.configured_urls();

    // --- the first run, under the fleet the scenario starts with
    let mode = match scenario.crash_stage() {
        Some(stage) => RunMode::Armed { stage },
        None => RunMode::ObserveHelperOutage,
    };
    let mut opening = config_for(
        fixture,
        &sidecar,
        round,
        mode,
        MAX_DISPATCHES,
        &Faults::fleet(first.clone()),
    );
    // Both runs here are unarmed, so both would otherwise write to the same
    // resume log and the second would truncate the first's record of which
    // helpers it reached. Reading the first run's contacts before the second
    // starts would work today and break the moment those lines are reordered,
    // so the two are given separate files instead.
    opening.crash_log = sidecar.with_extension("first.crashlog.jsonl");

    if scenario.crash_stage().is_some() {
        let crash = run_until_crash(&fixture.worker, &opening);
        warm_from(fixture, &sidecar);
        crash.map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    } else {
        // Cancellation after a delivery observation is the expected opening
        // boundary. A transport or setup failure is not evidence of an outage.
        let outcome = run_to_quiescence(&fixture.worker, &opening);
        warm_from(fixture, &sidecar);
        let outcome = outcome
            .map_err(|error| Outcome::Failed(format!("outage opening failed: {error:#}")))?;
        if outcome.quiescence_kind != "Cancelled" {
            return Err(Outcome::Failed(format!(
                "outage opening missed its delivery boundary: {}",
                outcome.quiescence_kind
            )));
        }
        eprintln!(
            "  {scenario}: first run ended at {}",
            outcome.quiescence_kind
        );
    }

    let after_first = DurableSnapshot::read(&sidecar)
        .map_err(|error| Outcome::Failed(format!("unreadable sidecar: {error:#}")))?;
    let first_contacts = contacts(&opening);
    if scenario == FleetScenario::WholeFleetDown
        && (first_contacts.refused.is_empty()
            || !first_contacts.answered.is_empty()
            || after_first.deliveries.is_empty()
            || after_first
                .deliveries
                .iter()
                .any(|delivery| delivery.confirmed || !delivery.sent.is_empty()))
    {
        return Err(Outcome::Failed(
            "whole-fleet outage did not leave observed, unaccepted helper work".into(),
        ));
    }
    eprintln!(
        "  {scenario}: first run placed {} share(s) with {} helper(s); answered {:?}, \
         refused {}, silent {}",
        after_first
            .deliveries
            .iter()
            .filter(|delivery| !delivery.sent.is_empty())
            .count(),
        first_contacts.attempted().len(),
        first_contacts.answered.len(),
        first_contacts.refused.len(),
        first_contacts.unanswered.len(),
    );

    // Whatever the fleet did, the wallet must have written down every helper it
    // reached, and reached nothing it was not configured with.
    assert_no_contact_outside_the_fleet(&first_urls, &first_contacts.attempted())
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    // Only the silent ones. A refusal is a definite pre-dispatch failure, and
    // the SDK is right to clear that reservation: no byte left, so there is
    // nothing to recover and a retained row would make the wallet poll a helper
    // that provably never received the share.
    assert_every_unanswered_helper_was_journalled(&after_first, &first_contacts.unanswered)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    assert_placement_stays_within_the_fleet(&after_first, &first_urls)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;

    // The anti-vacuity gate, and the reason this matrix can be believed at all.
    // Every scenario claims the *resumed* run does something; a first run that
    // finished the round would let every later assertion hold trivially against
    // a completed round. This suite has already been fooled that way once.
    if scenario.must_leave_work_outstanding() {
        let outstanding = after_first
            .deliveries
            .iter()
            .filter(|delivery| !delivery.confirmed)
            .count();
        let unplaced = after_first
            .deliveries
            .iter()
            .filter(|delivery| delivery.sent.is_empty())
            .count();
        eprintln!(
            "  {scenario}: first run left {outstanding} share(s) unconfirmed and {unplaced} \
             with no acceptance, of {}",
            after_first.deliveries.len()
        );
        if scenario.must_leave_acceptances_behind() {
            let accepted: usize = after_first
                .deliveries
                .iter()
                .map(|delivery| delivery.sent.len())
                .sum();
            if accepted == 0 {
                return Err(Outcome::Failed(format!(
                    "{scenario}: the first run recorded no acceptance at all, so the claim \
                     that acceptances survive the fleet changing under them is a claim \
                     about an empty set. The crash must land after a helper has taken a \
                     share, not before"
                )));
            }
            eprintln!("  {scenario}: first run left {accepted} acceptance(s) behind");
        }
        if outstanding == 0 {
            return Err(Outcome::Failed(format!(
                "{scenario}: the first run finished the round, so the resumed run had nothing \
                 to do and every assertion below would hold against a completed round. A \
                 share confirms at whatever placement it reaches, so under-delivery alone \
                 leaves no work; this scenario needs its first run cut short"
            )));
        }
    }

    if scenario.leaves_an_unknown_outcome() {
        let unknown: usize = after_first
            .deliveries
            .iter()
            .map(|delivery| delivery.ambiguous.len() + delivery.attempting.len())
            .sum();
        // A silent helper must leave evidence it was tried. Written off, the
        // wallet would either re-send blindly or believe it never contacted the
        // helper at all, and the share's real whereabouts would be unknown to it.
        if unknown == 0 && !first_contacts.unanswered.is_empty() {
            return Err(Outcome::Failed(format!(
                "{scenario}: {} helper(s) went silent and no share records an unresolved \
                 attempt; an outcome nobody can learn must be journaled, not discarded",
                first_contacts.unanswered.len()
            )));
        }
        eprintln!("  {scenario}: {unknown} unresolved attempt(s) journaled");
    }

    // --- the second run, under the fleet the scenario flips to
    let resumed = config_for(
        fixture,
        &sidecar,
        round,
        RunMode::Unarmed,
        MAX_DISPATCHES,
        &Faults::fleet(second.clone()),
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
            outcome.quiescence_kind, outcome.failures
        )));
    }

    let terminal = DurableSnapshot::read(&sidecar)
        .map_err(|error| Outcome::Failed(format!("unreadable sidecar: {error:#}")))?;
    // The resume's own record of which helpers it POSTed to. Durable state
    // shows where a share ended up; only this shows where a run declined to
    // send one again, which is the whole subject of the deficit rules.
    let resume_contacts = contacts(&resumed);
    let contacted: BTreeSet<String> = resume_contacts
        .attempted()
        .union(&tracking_contacts(&outcome))
        .cloned()
        .collect();
    eprintln!(
        "  {scenario}: resume contacted {} helper(s): {contacted:?}",
        contacted.len()
    );

    // --- the invariants the fleet exists to make checkable
    assert_acceptances_never_downgraded(&after_first, &terminal)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    assert_no_contact_outside_the_fleet(&second_urls, &contacted)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    // Per share, not fleet-wide: a run legitimately POSTs different shares to
    // the same helper, and only the SDK's own report carries the share identity
    // beside the helper it reached.
    assert_no_premature_resend_to_an_accepted_helper(
        &after_first,
        &second_urls,
        &tracked_contacts_per_share(&outcome),
    )
    .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    assert_every_unanswered_helper_was_journalled(&terminal, &resume_contacts.unanswered)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;
    assert_reservations_monotonic(&after_first, &terminal)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;

    // What the round owes: every share held and acknowledged, whatever the
    // fleet did while it was being placed.
    assert_every_share_is_confirmed(&terminal)
        .map_err(|error| Outcome::Failed(format!("{error:#}")))?;

    // Placement is reported, not asserted. The first live run established why:
    // a share confirms whether or not its fan-out reached `target_count`
    // helpers, and once confirmed nothing repairs the shortfall, because
    // tracking walks unconfirmed shares only. The spread is printed every run
    // so a change in effective redundancy stays visible even though no rule
    // names it.
    let (least, most) = placement_spread(&terminal);
    eprintln!(
        "  {scenario}: {} share(s) confirmed, each held by {least}..={most} helper(s), \
         initial-placement target {}",
        terminal.deliveries.len(),
        share_submission_target_count(second_urls.len())
    );

    // The round still has to be a round. Comparing against the control catches
    // a fleet scenario that converged by losing work rather than by doing it.
    if let Err(error) = recovery_conformance::assertions::assert_matches_control(&terminal, control)
    {
        return Err(Outcome::Failed(format!("{error:#}")));
    }
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

/// The helpers one run's crash log says it POSTed a share to.
fn contacts(config: &recovery_conformance::run_config::RoundRunConfig) -> HelperContacts {
    HelperContacts::from_observations(&CrashLog::read(&config.crash_log).unwrap_or_default())
}

/// The helpers background share tracking reported reaching, as one flat set.
///
/// Read alongside the crash log rather than instead of it. The log records what
/// the route saw; the SDK's own report records what it believes it did, and a
/// disagreement between the two would itself be worth knowing about.
///
/// Flat, so only usable for claims about the fleet as a whole — "nothing
/// outside the configured helpers was contacted", not "this share was not
/// re-sent". For the latter see [`tracked_contacts_per_share`].
fn tracking_contacts(outcome: &recovery_conformance::run_config::RunOutcome) -> BTreeSet<String> {
    outcome
        .share_tracking
        .iter()
        .flat_map(|summary| summary.contacted_urls())
        .collect()
}

/// The helpers tracking reached, keyed by the share it reached them for.
///
/// The distinction matters: a run POSTs many different shares to the same
/// helper, so a fleet-wide set would report a correct delivery of one share as
/// a forbidden re-send of another. The route's own contact log cannot supply
/// this — it sees a URL, not which share was in flight — so this comes from the
/// SDK's report alone.
fn tracked_contacts_per_share(
    outcome: &recovery_conformance::run_config::RunOutcome,
) -> BTreeMap<(i64, i64, i64), BTreeSet<String>> {
    let mut per_share: BTreeMap<(i64, i64, i64), BTreeSet<String>> = BTreeMap::new();
    for summary in &outcome.share_tracking {
        for record in summary.resubmitted.iter().chain(summary.ambiguous.iter()) {
            per_share
                .entry((
                    record.bundle_index as i64,
                    record.proposal_id as i64,
                    record.share_index as i64,
                ))
                .or_default()
                .insert(record.server_url.clone());
        }
    }
    per_share
}

/// The scenarios this run exercises, or `None` for all of them.
fn selected_scenarios() -> Option<Vec<FleetScenario>> {
    let requested = std::env::var("RECOVERY_CONFORMANCE_FLEET").ok()?;
    let scenarios: Vec<FleetScenario> = requested
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            name.parse::<FleetScenario>().unwrap_or_else(|_| {
                panic!(
                    "RECOVERY_CONFORMANCE_FLEET names an unknown scenario {name:?}; known \
                     scenarios are {}",
                    FleetScenario::ALL
                        .iter()
                        .map(|scenario| scenario.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
        })
        .collect();
    (!scenarios.is_empty()).then_some(scenarios)
}
