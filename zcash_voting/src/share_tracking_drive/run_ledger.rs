//! What one tracking run has accumulated, and how each pass folds into it.
//!
//! The ledger is the run's only mutable state. It owns nothing about *when* to
//! run a pass: it records what the passes did and turns itself into the report
//! once the reason for stopping is known.

use crate::share_tracking::{ResubmittedShare, ShareKey, ShareTrackingReport};

use super::{quiescence::ShareTrackingQuiescence, ShareTrackingRunReport};

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
        self.confirmed.extend(report.confirmed.iter().copied());
        self.resubmitted.extend(report.resubmitted.iter().cloned());
        self.ambiguous.extend(report.ambiguous.iter().cloned());
        // Replaced, not extended: a share can stop being unrecoverable once
        // its material is restored, and a host shown a stale union would keep
        // warning about a share that recovered.
        self.unrecoverable = report.unrecoverable.clone();
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
