//! Confirming one submitted helper share by polling the helpers that hold it.

use crate::{
    share_tracking::{confirm_pending_share, ShareConfirmationParams, ShareKey},
    ChainTransport,
};

use super::{
    step_ledger::StepLedger, step_scope::StepScope, RoundExecutor, RoundStepDisposition,
    RoundStepFailure, RoundStepOutcome, RoundStepProgress, RoundStepProgressReporter,
};

impl<T: ChainTransport> RoundExecutor<T> {
    pub(super) async fn run_confirm_share(
        &self,
        scope: &StepScope<'_>,
        share: ShareKey,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let ledger = StepLedger::default();
        let cancel = || scope.interrupted();
        let report = confirm_pending_share(
            &self.database,
            &ShareConfirmationParams {
                round_id: &scope.round_id,
                share,
                configured_server_urls: &scope.host.configured_helper_urls,
                now_seconds: scope.host.now_seconds,
            },
            &self.helper_client,
            &cancel,
        )
        .await
        .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
        progress.report(RoundStepProgress::ShareConfirmed {
            share,
            confirmed: report.confirmed,
        });
        let disposition = if report.confirmed {
            RoundStepDisposition::Advanced
        } else if scope.interrupted() {
            RoundStepDisposition::Cancelled
        } else {
            RoundStepDisposition::Pending
        };
        self.outcome(scope, disposition, ledger)
    }
}
