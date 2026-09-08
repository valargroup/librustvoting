//! What one tracking run has accumulated, and how each pass folds into it.
//!
//! The ledger is the run's only mutable state. It owns nothing about *when* to
//! run a pass: it records what the passes did and turns itself into the report
//! once the reason for stopping is known.

use crate::share_tracking::{ResubmittedShare, ShareKey, ShareTrackingReport};

use super::{quiescence::ShareTrackingQuiescence, ShareTrackingRunReport};

/// Shares a pass proved are not beyond repair.
///
/// A confirmation settles it outright. So does reaching a helper, whether the
/// acceptance was definite or ambiguous: recovery could only send a share it
/// still had the material for.
fn recovered_shares(report: &ShareTrackingReport) -> Vec<ShareKey> {
    report
        .confirmed
        .iter()
        .copied()
        .chain(report.resubmitted.iter().map(|attempt| attempt.share))
        .chain(report.ambiguous.iter().map(|attempt| attempt.share))
        .collect()
}

/// What the run has accumulated so far.
#[derive(Default)]
pub(super) struct Run {
    pub(super) passes: u32,
    confirmed: Vec<ShareKey>,
    resubmitted: Vec<ResubmittedShare>,
    ambiguous: Vec<ResubmittedShare>,
    unrecoverable: Vec<ShareKey>,
    pub(super) failures: Vec<String>,
}

impl Run {
    /// Folds one successful pass into the run.
    pub(super) fn absorb(&mut self, report: &ShareTrackingReport) {
        self.absorb_durable_effects(report);
        // Replaced, not extended: a share can stop being unrecoverable once
        // its material is restored, and a host shown a stale union would keep
        // warning about a share that recovered.
        self.unrecoverable = report.unrecoverable.clone();
    }

    /// Folds what a failed pass had already committed into the run.
    ///
    /// A pass writes as it walks, so its durable effects are real whether or
    /// not it reached the end. They are also unrepeatable: the next pass walks
    /// only unconfirmed shares, so a share this one confirmed is never seen
    /// again and would be missing from the run's report altogether.
    ///
    /// The retained `unrecoverable` set is kept rather than replaced. It is a
    /// statement about the whole round, taken from the last pass that saw
    /// every share; a partial walk saw a prefix, and letting it replace the
    /// set would drop shares whose material is still missing.
    ///
    /// Kept is not frozen, though: a share this pass confirmed or reached a
    /// helper with is no longer beyond repair, whatever an earlier pass
    /// concluded. Leaving those in would let one report call a share both
    /// recovered and unrecoverable, and let `PassBudgetExhausted` name a share
    /// that is already confirmed.
    pub(super) fn absorb_partial(&mut self, report: &ShareTrackingReport) {
        self.absorb_durable_effects(report);
        let recovered = recovered_shares(report);
        self.unrecoverable
            .retain(|share| !recovered.contains(share));
    }

    /// The three per-share effects a pass commits as it makes them.
    fn absorb_durable_effects(&mut self, report: &ShareTrackingReport) {
        self.confirmed.extend(report.confirmed.iter().copied());
        self.resubmitted.extend(report.resubmitted.iter().cloned());
        self.ambiguous.extend(report.ambiguous.iter().cloned());
    }

    /// The last `count` failures, oldest first.
    pub(super) fn recent_failures(&self, count: u32) -> Vec<String> {
        self.failures
            .iter()
            .rev()
            .take(count as usize)
            .rev()
            .cloned()
            .collect()
    }

    pub(super) fn finish(self, quiescence: ShareTrackingQuiescence) -> ShareTrackingRunReport {
        ShareTrackingRunReport {
            quiescence,
            passes: self.passes,
            confirmed: self.confirmed,
            resubmitted: self.resubmitted,
            ambiguous: self.ambiguous,
            unrecoverable: self.unrecoverable,
            failures: self.failures,
        }
    }

    /// Stops with the budget exhausted, naming the shares the last pass could
    /// not repair — the expected reason a run reaches the budget at all.
    pub(super) fn finish_exhausted(self) -> ShareTrackingRunReport {
        let unrecoverable = self.unrecoverable.clone();
        self.finish(ShareTrackingQuiescence::PassBudgetExhausted { unrecoverable })
    }
}
