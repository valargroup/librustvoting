//! Host control as seen by one round step.

use crate::ChainSubmissionControl;

/// The host's control captured when a step begins.
///
/// A step observes two interruption signals: explicit cancellation, and the
/// host moving to a new operation epoch (a session or account switch) after
/// the step began. Both are checked at every boundary where a step decides
/// whether to keep going, so a stale invocation never dispatches a vote or
/// helper share on behalf of an epoch the host has already left.
pub(super) struct StepControl<'a> {
    control: &'a ChainSubmissionControl,
    entry_epoch: u64,
}

impl<'a> StepControl<'a> {
    /// Captures the epoch the step starts under.
    pub(super) fn capture(control: &'a ChainSubmissionControl) -> Self {
        Self {
            control,
            entry_epoch: control.operation_epoch(),
        }
    }

    /// Whether the step must stop: the host cancelled, or it moved to another
    /// operation epoch since this step began.
    pub(super) fn interrupted(&self) -> bool {
        self.control.is_cancelled() || self.control.operation_epoch() != self.entry_epoch
    }

    /// The underlying control for chain submission and lock acquisition,
    /// which capture and enforce the epoch themselves.
    pub(super) fn chain(&self) -> &'a ChainSubmissionControl {
        self.control
    }
}
