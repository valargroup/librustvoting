//! Round setup, note selection, bundle layout, and eligibility preview.

use prost::Message as _;

use crate::backend::zcash_client_backend;
use zcash_client_backend::proto::service::TreeState;

use crate::{
    delegate::{self, DelegationRoundContext},
    note_bundling::MinimumVotingEligibility,
    round::BundleLayout,
    types::{NoteInfo, VotingError, VotingHotkey},
};

use super::{DelegationPipeline, WalletDbOpener};

/// Minimum voting eligibility plus the note value the privacy trim withholds.
///
/// Both come from one bundle plan, so the reported weight and the reported
/// loss describe the same note set. The withheld value is raw note value, not
/// bundle-quantized voting weight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VotingEligibilityReport {
    pub eligibility: MinimumVotingEligibility,
    pub privacy_trim_dropped_value_zatoshi: u64,
}

/// Decodes the anchor tree state a host fetched from lightwalletd.
///
/// The bytes are caller-supplied network output, so a decode failure is
/// [`VotingError::InvalidInput`]: the host refetches its tree state rather
/// than treating the failure as an SDK invariant violation. The pipeline
/// constructor runs this check so a malformed anchor is refused before any
/// stage starts.
pub(super) fn decode_anchor_tree_state(bytes: &[u8]) -> Result<TreeState, VotingError> {
    TreeState::decode(bytes).map_err(|e| VotingError::InvalidInput {
        message: format!("delegation anchor tree state bytes do not decode: {e}"),
    })
}

impl<W: WalletDbOpener> DelegationPipeline<W> {
    pub(super) fn hotkey(&self) -> Result<&VotingHotkey, VotingError> {
        self.hotkey
            .as_ref()
            .ok_or_else(|| VotingError::InvalidInput {
                message: "this delegation stage requires the round voting hotkey".to_string(),
            })
    }

    pub(super) fn anchor_tree_state(&self) -> Result<TreeState, VotingError> {
        decode_anchor_tree_state(&self.lwd.anchor_tree_state_bytes)
    }

    /// Ensures the round row exists and returns its display context.
    pub fn ensure_round(&self) -> Result<DelegationRoundContext, VotingError> {
        self.observe_ensure_round(&crate::ObservationScope::disabled())
    }

    pub(crate) fn observe_ensure_round(
        &self,
        observations: &crate::ObservationScope,
    ) -> Result<DelegationRoundContext, VotingError> {
        observations.bind_round_id(self.round_id());
        let attributed = observations.attributed(Default::default());
        let stage = attributed.stage("delegation::ensure_round");
        let result = self.execute_ensure_round(stage.scope());
        let outcome = if result.is_ok() {
            crate::ObservationOutcome::Succeeded
        } else {
            crate::ObservationOutcome::Failed
        };
        stage.finish(
            outcome,
            result
                .as_ref()
                .err()
                .map(crate::observability::voting_error_kind),
        );
        result
    }

    pub(crate) fn execute_ensure_round(
        &self,
        observations: &crate::ObservationScope,
    ) -> Result<DelegationRoundContext, VotingError> {
        delegate::observe_ensure_round_context(
            self.scoped_voting_db()?,
            self.lwd.network,
            &self.lwd.round_params,
            &self.lwd.resolved_round_name,
            self.session_json.as_deref(),
            observations,
        )
    }

    /// Selects the account's voting-eligible notes at the round snapshot.
    pub fn select_notes(&self) -> Result<Vec<NoteInfo>, VotingError> {
        self.observe_select_notes(&crate::ObservationScope::disabled())
    }

    pub(crate) fn observe_select_notes(
        &self,
        observations: &crate::ObservationScope,
    ) -> Result<Vec<NoteInfo>, VotingError> {
        observations.bind_round_id(self.round_id());
        let attributed = observations.attributed(Default::default());
        let stage = attributed.stage("delegation::select_notes");
        let result = self.execute_select_notes(stage.scope());
        let outcome = if result.is_ok() {
            crate::ObservationOutcome::Succeeded
        } else {
            crate::ObservationOutcome::Failed
        };
        stage.finish(
            outcome,
            result
                .as_ref()
                .err()
                .map(crate::observability::voting_error_kind),
        );
        result
    }

    pub(crate) fn execute_select_notes(
        &self,
        observations: &crate::ObservationScope,
    ) -> Result<Vec<NoteInfo>, VotingError> {
        let wallet = self.wallet.open_for_read()?;
        let selected = crate::selection::observe_select_notes_with_wallet_db(
            &wallet,
            self.lwd.network,
            &self.account_uuid,
            self.snapshot_height(),
            self.anchor_tree_state()?,
            observations,
        )?;
        Ok(selected.voting_note_infos())
    }

    /// Creates or validates the round's delegation bundle rows.
    ///
    /// Existing rows are reused only when they match the current eligible
    /// note set.
    pub fn setup_bundles(&self) -> Result<BundleLayout, VotingError> {
        self.observe_setup_bundles(&crate::ObservationScope::disabled())
    }

    pub(crate) fn observe_setup_bundles(
        &self,
        observations: &crate::ObservationScope,
    ) -> Result<BundleLayout, VotingError> {
        observations.bind_round_id(self.round_id());
        let attributed = observations.attributed(Default::default());
        let stage = attributed.stage("delegation::setup_bundles");
        let result = self.execute_setup_bundles(stage.scope());
        let outcome = if result.is_ok() {
            crate::ObservationOutcome::Succeeded
        } else {
            crate::ObservationOutcome::Failed
        };
        stage.finish(
            outcome,
            result
                .as_ref()
                .err()
                .map(crate::observability::voting_error_kind),
        );
        result
    }

    /// Runs this workflow with optional per-call diagnostics, including on errors.
    pub fn setup_bundles_with_report(
        &self,
        options: Option<crate::ObservabilityOptions>,
    ) -> crate::OperationReport<Result<BundleLayout, VotingError>> {
        let invocation = crate::ObservationScope::new(options).invocation();

        let result = self.observe_setup_bundles(invocation.scope());
        let outcome = if result.is_ok() {
            crate::ObservationOutcome::Succeeded
        } else {
            crate::ObservationOutcome::Failed
        };
        invocation.complete("setup_bundles", outcome, result)
    }

    pub(crate) fn execute_setup_bundles(
        &self,
        observations: &crate::ObservationScope,
    ) -> Result<BundleLayout, VotingError> {
        self.execute_ensure_round(observations)?;
        let notes = self.execute_select_notes(observations)?;
        self.scoped_voting_db()?
            .ensure_bundles_with_skipped_suffix_with_policy(
                self.round_id(),
                &notes,
                self.bundle_policy,
            )
    }

    /// Checks whether the account can vote without persisting anything.
    ///
    /// Once a round has a persisted plan, its stored policy is authoritative
    /// and is used instead of the pipeline's seed policy, so the preview
    /// describes the plan the round would actually derive.
    pub fn eligibility(&self) -> Result<VotingEligibilityReport, VotingError> {
        self.observe_eligibility(&crate::ObservationScope::disabled())
    }

    /// Checks eligibility with optional diagnostics, including wallet-selection failures.
    /// Uses the same read-only workflow and errors as [`Self::eligibility`].
    pub fn eligibility_with_report(
        &self,

        options: Option<crate::ObservabilityOptions>,
    ) -> crate::OperationReport<Result<VotingEligibilityReport, VotingError>> {
        let invocation = crate::ObservationScope::new(options).invocation();
        let result = self.observe_eligibility(invocation.scope());
        let outcome = if result.is_ok() {
            crate::ObservationOutcome::Succeeded
        } else {
            crate::ObservationOutcome::Failed
        };
        invocation.complete("delegation::eligibility", outcome, result)
    }

    pub(crate) fn observe_eligibility(
        &self,
        observations: &crate::ObservationScope,
    ) -> Result<VotingEligibilityReport, VotingError> {
        observations.bind_round_id(self.round_id());
        let attributed = observations.attributed(Default::default());
        let stage = attributed.stage("delegation::eligibility");
        let result = self.execute_eligibility(stage.scope());
        let outcome = if result.is_ok() {
            crate::ObservationOutcome::Succeeded
        } else {
            crate::ObservationOutcome::Failed
        };
        stage.finish(
            outcome,
            result
                .as_ref()
                .err()
                .map(crate::observability::voting_error_kind),
        );
        result
    }

    pub(crate) fn execute_eligibility(
        &self,
        observations: &crate::ObservationScope,
    ) -> Result<VotingEligibilityReport, VotingError> {
        let notes = self.execute_select_notes(observations)?;
        let policy = self
            .scoped_voting_db()?
            .effective_bundle_policy(self.round_id(), self.bundle_policy)?;
        let (eligibility, plan) =
            crate::note_bundling::minimum_voting_eligibility_and_plan_for_notes(&notes, policy)?;
        Ok(VotingEligibilityReport {
            eligibility,
            privacy_trim_dropped_value_zatoshi: plan.privacy_trim.dropped_value,
        })
    }
}
