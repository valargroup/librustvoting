//! Host control as seen by one round step.

use crate::ChainSubmissionControl;

/// The host's control captured when a step begins.
///
/// A step observes two interruption signals: explicit cancellation, and the
/// host moving to a new operation epoch (a session or account switch) after
/// the step began. Both are checked at every boundary where a step decides
/// whether to keep going, so a stale invocation never dispatches a vote or
/// helper share on behalf of an epoch the host has already left.
#[derive(Clone, Copy)]
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

    /// The underlying control for lock acquisition and chain submission.
    /// Chain episodes must also receive [`Self::entry_epoch`] so they do not
    /// recapture a newer epoch as their own.
    pub(super) fn chain(&self) -> &'a ChainSubmissionControl {
        self.control
    }

    /// The operation epoch the step began under.
    pub(super) fn entry_epoch(&self) -> u64 {
        self.entry_epoch
    }
}
