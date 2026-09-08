//! Driver-level observations, on top of what one pass reports.

use std::time::Duration;

use crate::share_tracking::ShareTrackingReport;

/// One observation from a tracking run.
///
/// A run is a sequence of whole passes, so every event names the pass it
/// belongs to. Unlike a round run there is no concurrency here: passes are
/// strictly sequential, because each one plans from the share rows the
/// previous one wrote.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ShareTrackingEvent {
    /// A pass is about to poll helpers. `pass` counts from 1.
    PassStarted { pass: u32 },
    /// A pass completed and its durable effects are written.
    PassFinished {
        pass: u32,
        report: Box<ShareTrackingReport>,
    },
    /// A pass returned an error. Nothing durable is implied either way: the
    /// pass writes as it goes, so earlier shares in it may have advanced.
    PassFailed { pass: u32, message: String },
    /// The driver is waiting before the next pass. `delay` is the pass's own
    /// computed delay, or the policy's failure retry after a failed pass.
    AwaitingNextPass { delay: Duration },
}

/// Synchronous observer for [`ShareTrackingEvent`].
///
/// Called from the driver's own task between passes, so an implementation
/// must not block.
pub trait ShareTrackingReporter: Send + Sync {
    fn report(&self, event: ShareTrackingEvent);
}

/// Reporter for hosts that need only the run report.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopShareTrackingReporter {}

impl ShareTrackingReporter for NoopShareTrackingReporter {
    fn report(&self, _event: ShareTrackingEvent) {}
}

/// Adapts a closure to [`ShareTrackingReporter`].
pub struct ShareTrackingReporterBridge<F> {
    report: F,
}

impl<F> ShareTrackingReporterBridge<F> {
    pub fn new(report: F) -> Self {
        Self { report }
    }
}

impl<F> ShareTrackingReporter for ShareTrackingReporterBridge<F>
where
    F: Fn(ShareTrackingEvent) + Send + Sync,
{
    fn report(&self, event: ShareTrackingEvent) {
        (self.report)(event);
    }
}
