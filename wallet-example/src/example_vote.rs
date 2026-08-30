use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use zcash_voting::prelude::{
    canonical_helper_url_list, commit_atomic_vote_batch, commit_batch, os_random_bytes,
    sync_vote_tree, track_pending_shares, van_witness, CommittedVote, DraftVote, HelperClient,
    NoopProgressReporter, SharePayload, SharePlan, ShareSubmissionReport, ShareSubmissionRequest,
    ShareTimingPolicy, ShareTrackingParams, ShareTrackingReport, SignedVoteBatch,
    SignedVoteCommitment, SignedVoteCommitments, VanWitness, VoteSigner, VoteSubmission, VotingDb,
    VotingHotkey,
};
use zcash_voting::share::policy::{plan_share_submissions, share_submission_random_bytes_required};

/// Inputs for deriving a Merkle witness for one confirmed delegation bundle.
///
/// `round_id` and `bundle_index` identify a bundle whose VAN position has
/// already been recorded after delegation confirmation. `vote_node_url` is the
/// vote-chain endpoint used to sync the vote-authority-note tree.
pub struct WalletVanWitnessRequest<'a> {
    pub round_id: &'a str,
    pub bundle_index: u32,
    pub vote_node_url: &'a str,
}

/// Inputs for committing one cast-vote for one proposal.
///
/// `draft` is the wallet's proposal choice. `van_witness` must come from the
/// confirmed bundle identified by `round_id` and `bundle_index`, and
/// `voting_hotkey` signs the cast-vote payload.
pub struct WalletVoteCommitRequest<'a> {
    pub round_id: &'a str,
    pub bundle_index: u32,
    pub draft: &'a DraftVote,
    pub van_witness: &'a VanWitness,
    pub voting_hotkey: &'a VotingHotkey,
}

/// Inputs for the historical batch-named singleton compatibility API.
///
/// `drafts` must contain exactly one item. Use
/// [`WalletAtomicVoteBatchRequest`] for multiple proposals.
pub struct WalletVoteCommitBatchRequest<'a> {
    pub round_id: &'a str,
    pub bundle_index: u32,
    pub drafts: &'a [DraftVote],
    pub van_witness: &'a VanWitness,
    pub voting_hotkey: &'a VotingHotkey,
}

/// Inputs for committing one bundle's cast-votes in one atomic transaction.
pub struct WalletAtomicVoteBatchRequest<'a> {
    pub round_id: &'a str,
    pub bundle_index: u32,
    pub drafts: &'a [DraftVote],
    pub van_witness: &'a VanWitness,
    pub voting_hotkey: &'a VotingHotkey,
}

/// Persist this complete plan set before submitting its first helper share.
///
/// Keeping the canonical fleet with the plans lets restart recovery reuse the
/// same commitment-wide balancing and quota context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletHelperSharePlan {
    pub configured_server_urls: Vec<String>,
    pub share_plans: Vec<SharePlan>,
}

/// Inputs for planning all helper shares in one committed vote.
pub struct WalletHelperSharePlanningRequest<'a> {
    pub configured_server_urls: &'a [String],
    pub now_seconds: u64,
    pub vote_end_time_seconds: u64,
    pub last_moment_buffer_seconds: Option<u64>,
    pub single_share: bool,
    /// Position in this committed vote's share payloads, not a domain share ID.
    pub immediate_share_index: Option<u32>,
}

/// One durably journaled initial helper-share delivery result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalletShareDelivery {
    pub share_index: u32,
    pub submission: ShareSubmissionReport,
}

/// Inputs for one confirmation and recovery pass over a round.
pub struct WalletShareTrackingRequest<'a> {
    pub round_id: &'a str,
    /// The complete current helper fleet, which may differ from initial plans.
    pub configured_server_urls: &'a [String],
    pub now_seconds: u64,
    pub vote_end_time_seconds: Option<u64>,
}

/// Network-side results to persist after the caller submits a committed vote.
pub struct WalletVoteExecutionRequest<'a> {
    pub vote_tx_hash: &'a str,
    pub vc_tree_position: u64,
}

/// Chain and helper-server payloads derived from a committed vote.
pub struct WalletVoteExecutionPayload<'a> {
    pub submission: VoteSubmission,
    pub share_payloads: &'a [SharePayload],
}

/// Derives the VAN witness needed to commit votes for one bundle.
///
/// Call this after the bundle's delegation transaction is confirmed and its VAN
/// position is persisted. The returned witness is anchored at the vote-tree
/// height reached by the sync performed inside this function.
///
/// # Errors
///
/// Returns an error if the vote tree cannot be synced from `vote_node_url`, the
/// bundle is unknown, its VAN position or initial commitment is missing, its
/// confirmed position contains a different public leaf, or the witness cannot
/// be generated at the synced height.
pub fn derive_vote_van_witness(
    voting_db: &VotingDb,
    request: WalletVanWitnessRequest<'_>,
) -> Result<VanWitness> {
    // 1. Pull the latest vote-authority-note tree state for the round and keep
    // the synced height as the anchor for the witness produced below.
    let anchor_height = sync_vote_tree(voting_db, request.round_id, request.vote_node_url)
        .context("sync vote tree")?;

    // 2. Derive this bundle's Merkle witness against the freshly synced tree.
    van_witness(
        voting_db,
        request.round_id,
        request.bundle_index,
        anchor_height,
    )
    .context("generate VAN witness")
}

/// Builds, signs, and persists recovery state for one cast-vote.
///
/// The returned `CommittedVote` contains the chain-ready cast-vote fields and
/// helper-share payloads. Repeated calls for the same
/// `(round_id, bundle_index, proposal_id)` return persisted recovery material
/// when the stored draft matches the requested draft.
///
/// # Errors
///
/// Returns an error if the bundle or warmed voting state is missing, the draft
/// conflicts with stored recovery state, proof generation fails, signing fails,
/// or recovery state cannot be persisted.
pub fn commit_vote_bundle(
    voting_db: &VotingDb,
    request: WalletVoteCommitRequest<'_>,
) -> Result<CommittedVote> {
    let progress = NoopProgressReporter;
    CommittedVote::commit(
        voting_db,
        request.round_id,
        request.bundle_index,
        request.draft,
        request.van_witness,
        VoteSigner::hotkey(request.voting_hotkey),
        &progress,
    )
    .context("commit cast-vote")
}

/// Builds one signed commitment through the historical batch-named API.
///
/// `request.drafts` must contain exactly one item. Submit the returned
/// commitment to the singleton cast-vote endpoint.
pub fn commit_vote_bundle_batch(
    voting_db: &VotingDb,
    request: WalletVoteCommitBatchRequest<'_>,
) -> Result<SignedVoteCommitments> {
    let progress = NoopProgressReporter;
    commit_batch(
        voting_db,
        request.round_id,
        request.bundle_index,
        request.drafts,
        request.van_witness,
        VoteSigner::hotkey(request.voting_hotkey),
        &progress,
    )
    .context("commit independent cast-votes")
}

/// Builds, signs, and persists recovery state for one atomic cast-vote batch.
///
/// The returned value contains one canonical batch request body. Its individual
/// commitments must not be submitted through the singleton endpoint.
pub fn commit_atomic_vote_bundle_batch(
    voting_db: &VotingDb,
    request: WalletAtomicVoteBatchRequest<'_>,
) -> Result<SignedVoteBatch> {
    let progress = NoopProgressReporter;
    commit_atomic_vote_batch(
        voting_db,
        request.round_id,
        request.bundle_index,
        request.drafts,
        request.van_witness,
        VoteSigner::hotkey(request.voting_hotkey),
        &progress,
    )
    .context("commit atomic cast-vote batch")
}

/// Returns the payloads the caller should submit to external services.
///
/// Submit `submission` to the vote chain and each `share_payloads` item to the
/// selected helper server(s). Network requests remain caller-owned; the wallet
/// library only reconstructs the persisted payload fields.
pub fn committed_vote_payloads<'a>(
    voting_db: &VotingDb,
    committed: &'a CommittedVote,
) -> Result<WalletVoteExecutionPayload<'a>> {
    Ok(WalletVoteExecutionPayload {
        submission: committed
            .submission(voting_db)
            .context("build vote submission")?,
        share_payloads: committed.share_payloads(),
    })
}

/// Returns a single wallet-facing aggregate for external API boundaries.
///
/// This includes chain submission fields, helper-share payloads, and the stored
/// recovery JSON in one typed object.
pub fn committed_vote_signed_commitment(
    voting_db: &VotingDb,
    committed: &CommittedVote,
) -> Result<SignedVoteCommitment> {
    committed
        .signed_commitment(voting_db)
        .context("build signed vote commitment")
}

/// Plans every helper share together using fresh independent OS entropy.
///
/// The helper URLs are canonicalized before planning because placement counts
/// distinct endpoint identities. Persist the returned value before calling
/// [`submit_committed_vote_shares`] and reuse it after restart; planning only
/// missing shares would lose commitment-wide balancing and quota context.
pub fn plan_committed_vote_shares(
    committed: &CommittedVote,
    request: WalletHelperSharePlanningRequest<'_>,
) -> Result<WalletHelperSharePlan> {
    let configured_server_urls = canonical_helper_url_list(request.configured_server_urls)
        .context("validate helper URLs")?;
    let share_count = committed.share_payloads().len();
    let required = share_submission_random_bytes_required(
        share_count,
        configured_server_urls.len(),
        request.now_seconds,
        request.vote_end_time_seconds,
        request.last_moment_buffer_seconds,
        request.single_share,
    );
    let submit_at_random_bytes = os_random_bytes(required.submit_at_random_bytes);
    let server_random_bytes = os_random_bytes(required.server_random_bytes);
    let share_plans = plan_share_submissions(
        share_count,
        &configured_server_urls,
        request.now_seconds,
        request.vote_end_time_seconds,
        request.last_moment_buffer_seconds,
        request.single_share,
        request.immediate_share_index,
        &submit_at_random_bytes,
        &server_random_bytes,
    )
    .context("plan helper-share submissions")?;

    Ok(WalletHelperSharePlan {
        configured_server_urls,
        share_plans,
    })
}

/// Submits every committed helper share through crate-owned durable journaling.
///
/// `client` is caller-owned so a wallet can enforce its Tor or proxy route.
/// Each returned report describes state already persisted by
/// `CommittedVote::submit_share_to_helpers`; do not record it a second time.
pub async fn submit_committed_vote_shares(
    voting_db: &VotingDb,
    committed: &CommittedVote,
    client: &HelperClient,
    persisted_plan: &WalletHelperSharePlan,
    now_seconds: u64,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> Result<Vec<WalletShareDelivery>> {
    let share_count = committed.share_payloads().len();
    if persisted_plan.share_plans.len() != share_count {
        bail!(
            "persisted helper-share plan count {} does not match committed share count {share_count}",
            persisted_plan.share_plans.len()
        );
    }

    let mut deliveries = Vec::with_capacity(share_count);
    for (share_index, plan) in persisted_plan.share_plans.iter().enumerate() {
        if cancel() {
            break;
        }
        let share_index = u32::try_from(share_index).context("share index exceeds u32")?;
        let submission = committed
            .submit_share_to_helpers(
                voting_db,
                client,
                ShareSubmissionRequest {
                    share_index,
                    plan,
                    configured_server_urls: &persisted_plan.configured_server_urls,
                    now_seconds,
                },
                cancel,
            )
            .await
            .with_context(|| format!("submit helper share {share_index}"))?;
        deliveries.push(WalletShareDelivery {
            share_index,
            submission,
        });
    }

    Ok(deliveries)
}

/// Runs one crate-owned confirmation and recovery pass.
///
/// Schedule the next pass from `next_delay_seconds`. The caller retains
/// ownership of the timer, app-lock and round-expiry cancellation lifecycle.
pub async fn track_committed_vote_shares(
    voting_db: &VotingDb,
    client: &HelperClient,
    request: WalletShareTrackingRequest<'_>,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> Result<ShareTrackingReport> {
    let params = ShareTrackingParams {
        round_id: request.round_id,
        configured_server_urls: request.configured_server_urls,
        now_seconds: request.now_seconds,
        vote_end_time_seconds: request.vote_end_time_seconds,
        policy: ShareTimingPolicy::default(),
        random_bytes: &os_random_bytes,
    };
    track_pending_shares(voting_db, &params, client, cancel)
        .await
        .context("track pending helper shares")
}

/// Persists successful vote-chain submission fields.
///
/// Helper-share submission persists each attempt inside
/// `CommittedVote::submit_share_to_helpers`; it is intentionally absent here
/// so a wallet cannot accidentally record a post-hoc result twice.
pub fn record_committed_vote_execution(
    voting_db: &VotingDb,
    committed: &CommittedVote,
    request: WalletVoteExecutionRequest<'_>,
) -> Result<()> {
    committed
        .record_submission(voting_db, request.vote_tx_hash)
        .context("record vote submission")?;
    committed
        .record_vc_position(voting_db, request.vc_tree_position)
        .context("record vote commitment tree position")?;

    Ok(())
}
