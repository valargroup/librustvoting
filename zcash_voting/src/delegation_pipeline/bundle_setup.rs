//! Round setup, note selection, bundle layout, and eligibility preview.

use prost::Message as _;

use crate::backend::zcash_client_backend;
use zcash_client_backend::proto::service::TreeState;

use crate::{
    delegate::{self, DelegationRoundContext},
    note_bundling::MinimumVotingEligibility,
    round::BundleLayout,
    selection::select_notes_with_wallet_db,
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

impl<W: WalletDbOpener> DelegationPipeline<W> {
    pub(super) fn hotkey(&self) -> Result<&VotingHotkey, VotingError> {
        self.hotkey
            .as_ref()
            .ok_or_else(|| VotingError::InvalidInput {
                message: "this delegation stage requires the round voting hotkey".to_string(),
            })
    }

    pub(super) fn anchor_tree_state(&self) -> Result<TreeState, VotingError> {
        TreeState::decode(self.lwd.anchor_tree_state_bytes.as_slice()).map_err(|e| {
            VotingError::Internal {
                message: format!("failed to decode delegation anchor tree state: {e}"),
            }
        })
    }

    /// Ensures the round row exists and returns its display context.
    pub fn ensure_round(&self) -> Result<DelegationRoundContext, VotingError> {
        delegate::ensure_round_context(
            self.scoped_voting_db()?,
            self.lwd.network,
            &self.lwd.round_params,
            &self.lwd.resolved_round_name,
            self.session_json.as_deref(),
        )
    }

    /// Selects the account's voting-eligible notes at the round snapshot.
    pub fn select_notes(&self) -> Result<Vec<NoteInfo>, VotingError> {
        let wallet = self.wallet.open_for_read()?;
        let selected = select_notes_with_wallet_db(
            &wallet,
            self.lwd.network,
            &self.account_uuid,
            self.snapshot_height(),
            self.anchor_tree_state()?,
        )?;
        Ok(selected.voting_note_infos())
    }

    /// Creates or validates the round's delegation bundle rows.
    ///
    /// Existing rows are reused only when they match the current eligible
    /// note set.
    pub fn setup_bundles(&self) -> Result<BundleLayout, VotingError> {
        self.ensure_round()?;
        let notes = self.select_notes()?;
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
        let notes = self.select_notes()?;
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
