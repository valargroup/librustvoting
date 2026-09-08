//! What one round step runs under, captured once at entry.
//!
//! A step reads its wallet, round, roster, network, hotkey material, host
//! inputs and operation epoch from this scope for its whole duration. It
//! never re-reads the binding part-way through, so a long proof cannot
//! finish under different facts than it started with.

use zeroize::Zeroizing;

use crate::{
    session::NextStep, vote::CommittedVote, ChainSubmissionControl, ChainTransport, Network,
    VotingError, MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES,
};

use super::{
    step_control::StepControl, step_ledger::StepLedger, ProposalRosterEntry, RoundExecutor,
    RoundHostContext, RoundStepFailure, VoteRecoveryKey,
};

/// The captured scope of one step.
pub(super) struct StepScope<'a> {
    pub(super) observations: crate::ObservationScope,
    pub(super) step: NextStep,
    pub(super) wallet_id: String,
    /// Canonical lowercase-hex round id, and its bytes.
    pub(super) round_id: String,
    pub(super) round_id_bytes: [u8; 32],
    pub(super) network: Network,
    pub(super) proposals: Vec<ProposalRosterEntry>,
    /// The bound voting hotkey's stored secret, zeroized with the scope.
    pub(super) hotkey_secret: Option<Zeroizing<Vec<u8>>>,
    pub(super) host: &'a RoundHostContext,
    control: StepControl<'a>,
}

impl<'a> StepScope<'a> {
    /// Captures the scope `step` runs under: the executor's frozen wallet,
    /// its binding, and the control's entry epoch. Fails before any lock or
    /// I/O when the executor is unbound or its wallet handle drifted.
    pub(super) fn capture<T: ChainTransport>(
        executor: &RoundExecutor<T>,
        step: NextStep,
        host: &'a RoundHostContext,
        control: StepControl<'a>,
    ) -> Result<Self, RoundStepFailure> {
        let ledger = StepLedger::default();
        let wallet_id = executor
            .wallet_scope()
            .map(str::to_string)
            .map_err(|error| executor.step_voting_failure(error, Some(&step), &ledger))?;
        let binding = executor
            .binding()
            .map_err(|error| executor.step_voting_failure(error, Some(&step), &ledger))?;
        let round_id_bytes = parse_round_id(&binding.round_id)
            .map_err(|error| executor.step_voting_failure(error, Some(&step), &ledger))?;
        Ok(Self {
            observations: control
                .chain()
                .observations
                .clone()
                .unwrap_or_else(crate::ObservationScope::disabled),
            step,
            wallet_id,
            round_id: binding.round_id.clone(),
            round_id_bytes,
            network: binding.network,
            proposals: binding.proposals.clone(),
            hotkey_secret: binding.hotkey_secret.clone(),
            host,
            control,
        })
    }

    pub(super) fn proposal_ids(&self) -> Vec<u32> {
        self.proposals
            .iter()
            .map(|entry| entry.proposal_id)
            .collect()
    }

    pub(super) fn num_options(&self, proposal_id: u32) -> Option<u32> {
        self.proposals
            .iter()
            .find(|entry| entry.proposal_id == proposal_id)
            .map(|entry| entry.num_options)
    }

    /// Whether the step must stop: the host cancelled, or it moved to another
    /// operation epoch since this step began.
    pub(super) fn interrupted(&self) -> bool {
        self.control.interrupted()
    }

    /// The underlying control for lock acquisition and chain submission.
    pub(super) fn chain(&self) -> &'a ChainSubmissionControl {
        self.control.chain()
    }

    /// The operation epoch the step began under.
    pub(super) fn entry_epoch(&self) -> u64 {
        self.control.entry_epoch()
    }
}

/// The durable identity of `vote`, as progress and delivery reports name it.
pub(super) fn vote_key(vote: &CommittedVote) -> VoteRecoveryKey {
    VoteRecoveryKey {
        bundle_index: vote.bundle_index(),
        proposal_id: vote.proposal_id(),
    }
}

/// Decodes a canonical lowercase-hex round id into its 32 bytes, refusing
/// any other spelling with [`VotingError::InvalidInput`].
pub(super) fn parse_round_id(round_id: &str) -> Result<[u8; 32], VotingError> {
    crate::types::validate_vote_round_id_hex(round_id)?;
    let bytes = hex::decode(round_id).map_err(|error| VotingError::InvalidInput {
        message: format!("vote_round_id is not valid hex: {error}"),
    })?;
    bytes
        .try_into()
        .map_err(|bytes: Vec<u8>| VotingError::InvalidInput {
            message: format!("vote_round_id must be 32 bytes, got {}", bytes.len()),
        })
}

/// Escapes control characters and truncates `message` to the chain
/// submission diagnostic budget, so a failure message never smuggles a
/// newline or an unbounded response body into host logs.
pub(super) fn bounded_message(message: &str) -> String {
    let mut bounded =
        String::with_capacity(message.len().min(MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES));
    for character in message.chars() {
        let escaped = character.escape_default().collect::<String>();
        if bounded.len() + escaped.len() > MAX_CHAIN_SUBMISSION_DIAGNOSTIC_BYTES {
            break;
        }
        bounded.push_str(&escaped);
    }
    bounded
}
