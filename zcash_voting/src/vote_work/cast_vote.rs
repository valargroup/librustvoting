//! Casting a bundle's drafts: vote-tree sync with node failover, VAN witness,
//! vote proving and persistence, then hand-off to vote completion.

use std::sync::Arc;

use crate::{
    session::{NextStep, RoundPlan},
    types::VoteCommitStageBridge,
    vote::{
        persist_prepared_vote_work, prepare_vote_work, validate_draft_votes, CommittedVote,
        SignedVoteBatch, VoteCommitStage, VoteCommitmentRecovery, VoteSigner, VoteWorkRequest,
    },
    wire::DraftVote,
    ChainTransport, Network, VotingError, VotingHotkey,
};

use super::{
    execution::bounded_message, step_control::StepControl, steps::PROVING_STACK_BYTES,
    RoundExecutor, RoundHostContext, RoundStepFailure, RoundStepFailureKind, RoundStepOutcome,
    RoundStepProgress, RoundStepProgressReporter,
};

impl<T: ChainTransport> RoundExecutor<T> {
    pub(super) async fn run_cast_vote(
        &self,
        step: NextStep,
        bundle_index: u32,
        plan: &RoundPlan,
        host: &RoundHostContext,
        control: &StepControl<'_>,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let binding = self
            .binding()
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        let single_share = host.is_last_moment();
        let mut drafts = Vec::new();
        for planned in &plan.next_steps {
            if let NextStep::CastVote {
                bundle_index: planned_bundle,
                proposal_id,
                choice,
            } = planned
            {
                if *planned_bundle != bundle_index {
                    continue;
                }
                let num_options = binding.num_options(*proposal_id).ok_or_else(|| {
                    self.step_failure(
                        RoundStepFailureKind::InvalidInput,
                        Some(step.clone()),
                        None,
                        None,
                        format!("proposal {proposal_id} is not in the round roster"),
                    )
                })?;
                drafts.push(DraftVote {
                    proposal_id: *proposal_id,
                    choice: *choice,
                    num_options,
                    vc_tree_position: 0,
                    single_share,
                });
            }
        }
        validate_draft_votes(&drafts)
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        let hotkey_secret = binding.hotkey_secret.clone().ok_or_else(|| {
            self.step_failure(
                RoundStepFailureKind::InvalidInput,
                Some(step.clone()),
                None,
                None,
                "casting a vote requires the round voting hotkey",
            )
        })?;
        let network = binding.network;
        let round_id = binding.round_id.clone();

        // Tree sync and witness generation block on their own HTTP runtime.
        // Nodes are tried in order. Every failed sync drops the round's cached
        // tree, including the last node's, so neither the next node nor the
        // next pass inherits a partially appended or mismatched tree.
        validate_vote_tree_node_urls(&host.vote_tree_node_urls, network)
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        // One handle serves the sync, any reset, and the witness, so another
        // executor rebinding this wallet's transport in between cannot hand
        // the witness a client without the round state just synced.
        let tree = {
            let db = Arc::clone(&self.database);
            let transport = self.tree_transport.clone();
            self.blocking(&step, "vote tree binding", move || {
                crate::precompute::vote_tree_for(&db, transport)
            })
            .await?
        };
        let mut height = None;
        let mut last_failure = None;
        for node_url in &host.vote_tree_node_urls {
            let db = Arc::clone(&self.database);
            let sync_tree = Arc::clone(&tree);
            let sync_round_id = round_id.clone();
            let node_url = node_url.clone();
            let synced = self
                .blocking(&step, "vote tree sync", move || {
                    sync_tree.sync(&db, &sync_round_id, &node_url)
                })
                .await;
            match synced {
                Ok(synced) => {
                    height = Some(synced);
                    break;
                }
                Err(mut failure) => {
                    let reset_tree = Arc::clone(&tree);
                    let reset_round_id = round_id.clone();
                    if let Err(reset_failure) = self
                        .blocking(&step, "vote tree reset", move || {
                            reset_tree.reset(&reset_round_id)
                        })
                        .await
                    {
                        failure.message = bounded_message(&format!(
                            "{}; resetting the cached vote tree also failed: {}",
                            failure.message, reset_failure.message
                        ));
                    }
                    // A host that cancelled while the request was in flight
                    // asked for the step to stop; report that, not the
                    // transport error the cancellation produced.
                    if control.interrupted() {
                        return self.step_cancelled(Some(step), None, Vec::new(), None);
                    }
                    last_failure = Some(failure);
                }
            }
        }
        let Some(height) = height else {
            return Err(last_failure.expect("at least one node URL was tried"));
        };
        progress.report(RoundStepProgress::TreeSynced { height });
        let witness = {
            let db = Arc::clone(&self.database);
            let witness_tree = Arc::clone(&tree);
            let witness_round_id = round_id.clone();
            self.blocking(&step, "VAN witness", move || {
                witness_tree.generate_van_witness(&db, &witness_round_id, bundle_index, height)
            })
            .await?
        };
        if control.interrupted() {
            return self.step_cancelled(Some(step), None, Vec::new(), None);
        }

        // Proving runs on a dedicated large-stack thread; stages stream back.
        let (stage_tx, mut stage_rx) = tokio::sync::mpsc::unbounded_channel::<VoteCommitStage>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let db = Arc::clone(&self.database);
        let max_proof_concurrency = host.max_proof_concurrency.max(1);
        std::thread::Builder::new()
            .name("voting-vote-commit".to_string())
            .stack_size(PROVING_STACK_BYTES)
            .spawn(move || {
                let result = (|| {
                    let hotkey =
                        VotingHotkey::from_stored_secret(hotkey_secret.as_slice(), network)?;
                    let stages = VoteCommitStageBridge::new(move |stage| {
                        let _ = stage_tx.send(stage);
                    });
                    let prepared = prepare_vote_work(
                        &db,
                        VoteSigner::hotkey(&hotkey),
                        VoteWorkRequest {
                            round_id: &round_id,
                            bundle_index,
                            drafts: &drafts,
                            witness: &witness,
                            stages: &stages,
                            max_proof_concurrency,
                        },
                    )?;
                    persist_prepared_vote_work(&db, prepared)
                })();
                let _ = done_tx.send(result);
            })
            .map_err(|error| {
                self.step_failure(
                    RoundStepFailureKind::InvariantViolation,
                    Some(step.clone()),
                    None,
                    None,
                    format!("failed to spawn vote commit thread: {error}"),
                )
            })?;
        tokio::pin!(done_rx);
        let recovery = loop {
            tokio::select! {
                Some(stage) = stage_rx.recv() => {
                    progress.report(RoundStepProgress::VoteCommit(stage));
                }
                result = &mut done_rx => {
                    break result.map_err(|_| {
                        self.step_failure(
                            RoundStepFailureKind::InvariantViolation,
                            Some(step.clone()),
                            None,
                            None,
                            "vote commit thread exited without a result",
                        )
                    })?;
                }
            }
        };
        while let Ok(stage) = stage_rx.try_recv() {
            progress.report(RoundStepProgress::VoteCommit(stage));
        }
        let recovery =
            recovery.map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;

        let round_id = self
            .binding()
            .map(|b| b.round_id.clone())
            .unwrap_or_default();
        let (votes, batch) = self
            .recover_committed(&round_id, recovery)
            .map_err(|error| self.step_voting_failure(error, Some(step.clone())))?;
        self.finish_vote_work(
            step,
            votes,
            batch,
            true,
            &host.chain_policy,
            host,
            control,
            progress,
        )
        .await
    }

    pub(super) fn recover_committed(
        &self,
        round_id: &str,
        recovery: VoteCommitmentRecovery,
    ) -> Result<(Vec<CommittedVote>, Option<SignedVoteBatch>), VotingError> {
        let bundle_index = recovery.bundle_index();
        let votes = recovery
            .proposal_ids()
            .into_iter()
            .map(|proposal_id| {
                CommittedVote::recover(&self.database, round_id, bundle_index, proposal_id)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch = match recovery {
            VoteCommitmentRecovery::AtomicBatch(batch) => Some(batch),
            _ => None,
        };
        Ok((votes, batch))
    }
}

/// Validates the complete vote-tree node list before any sync.
///
/// Every URL must parse with an `http` or `https` scheme and a host, and on
/// Mainnet every URL must use HTTPS, matching the chain client's endpoint
/// rule: a plaintext tree endpoint would expose the wallet's round-specific
/// traffic and let an on-path attacker serve a forged tree that drives
/// expensive proving before the chain rejects the anchor.
pub(super) fn validate_vote_tree_node_urls(
    node_urls: &[String],
    network: Network,
) -> Result<(), VotingError> {
    if node_urls.is_empty() {
        return Err(VotingError::InvalidInput {
            message: "casting a vote requires at least one vote-tree node URL".to_string(),
        });
    }
    for node_url in node_urls {
        let uri: http::Uri = node_url
            .parse()
            .map_err(|error| VotingError::InvalidInput {
                message: format!("vote-tree node URL {node_url:?} is invalid: {error}"),
            })?;
        let scheme = uri.scheme_str().unwrap_or_default();
        if !matches!(scheme, "http" | "https") || uri.host().is_none() {
            return Err(VotingError::InvalidInput {
                message: format!(
                    "vote-tree node URL {node_url:?} must be an http or https URL with a host"
                ),
            });
        }
        if network == Network::Mainnet && scheme != "https" {
            return Err(VotingError::InvalidInput {
                message: format!("production vote-tree node URL {node_url:?} must use HTTPS"),
            });
        }
    }
    Ok(())
}
