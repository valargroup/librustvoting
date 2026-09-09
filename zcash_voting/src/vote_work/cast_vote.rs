//! Casting a bundle's drafts: vote-tree sync with node failover, VAN witness,
//! vote proving and persistence, then hand-off to vote completion.

use std::sync::Arc;

use crate::{
    round_planning::CastDraft,
    types::VoteCommitStageBridge,
    vote::{
        validate_draft_votes, CommittedVote, SignedVoteBatch, VoteCommitStage,
        VoteCommitmentRecovery, VoteSigner, VoteWorkRequest,
    },
    wire::DraftVote,
    ChainTransport, Network, VotingError, VotingHotkey,
};

use super::{
    round_lock::HeldRoundLock, step_ledger::StepLedger, step_scope::StepScope,
    vote_completion::CompletionEntry, RoundExecutor, RoundStepFailure, RoundStepFailureKind,
    RoundStepOutcome, RoundStepProgress, RoundStepProgressReporter,
};

impl<T: ChainTransport> RoundExecutor<T> {
    /// Casts every draft of `bundle_index` as one unit.
    pub(super) async fn run_cast_vote(
        &self,
        scope: &StepScope<'_>,
        bundle_index: u32,
        drafts: &[CastDraft],
        lock: &HeldRoundLock,
        progress: &dyn RoundStepProgressReporter,
    ) -> Result<RoundStepOutcome, RoundStepFailure> {
        let ledger = StepLedger::default();
        let host = scope.host;
        // A fresh vote after the authenticated deadline would sync the tree,
        // prove, persist recovery material, and submit only to be rejected;
        // refuse it here. Advancing or recovering work already on the wire
        // is not a cast and is unaffected.
        if let Some(vote_end) = host.vote_end_time_seconds {
            if host.now_seconds >= vote_end {
                return Err(self.step_failure(
                    RoundStepFailureKind::VoteEnded,
                    Some(&scope.step),
                    None,
                    &ledger,
                    format!(
                        "vote ended at {vote_end} and the host clock reads {}; a new vote cannot be cast",
                        host.now_seconds
                    ),
                ));
            }
        }
        let single_share = host.is_last_moment();
        let mut draft_votes = Vec::with_capacity(drafts.len());
        for draft in drafts {
            let num_options = scope.num_options(draft.proposal_id).ok_or_else(|| {
                self.step_failure(
                    RoundStepFailureKind::InvalidInput,
                    Some(&scope.step),
                    None,
                    &ledger,
                    format!("proposal {} is not in the round roster", draft.proposal_id),
                )
            })?;
            draft_votes.push(DraftVote {
                proposal_id: draft.proposal_id,
                choice: draft.choice,
                num_options,
                vc_tree_position: 0,
                single_share,
            });
        }
        let drafts = draft_votes;
        validate_draft_votes(&drafts)
            .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
        let hotkey_secret = scope.hotkey_secret.clone().ok_or_else(|| {
            self.step_failure(
                RoundStepFailureKind::InvalidInput,
                Some(&scope.step),
                None,
                &ledger,
                "casting a vote requires the round voting hotkey",
            )
        })?;
        let network = scope.network;
        let round_id = scope.round_id.clone();
        // The bound hotkey must be the one the bundle's delegation targets;
        // otherwise ZKP #2 would fail only after a tree sync and a witness.
        let bound_target = VotingHotkey::from_stored_secret(hotkey_secret.as_slice(), network)
            .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?
            .delegation_target();
        self.database
            .validate_bundle_hotkey_target(&round_id, bundle_index, &bound_target)
            .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
        // A committed but undispatched vote on this bundle for a proposal the
        // roster no longer lists can never be submitted and would otherwise
        // reserve the bundle's VAN against this cast. The retirement re-checks
        // the durable phase inside its own write transaction.
        self.database
            .retire_undispatched_votes_outside_roster(
                &round_id,
                bundle_index,
                &scope.proposal_ids(),
            )
            .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;

        if self
            .database
            .delegation_phase(&round_id, bundle_index)
            .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?
            != crate::phases::DelegationPhase::Confirmed
        {
            return self
                .run_delegate_and_vote_batch(scope, bundle_index, drafts, lock, progress)
                .await;
        }

        // Tree sync and witness generation block on their own HTTP runtime.
        // Nodes are tried in order. Every failed sync drops the round's cached
        // tree, including the last node's, so neither the next node nor the
        // next pass inherits a partially appended or mismatched tree.
        let node_urls = canonical_vote_tree_node_urls(&host.vote_tree_node_urls, network)
            .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
        // One handle serves the sync, any reset, and the witness, so another
        // executor rebinding this wallet's transport in between cannot hand
        // the witness a client without the round state just synced.
        let tree = {
            let db = Arc::clone(&self.database);
            let transport = self.tree_transport.clone();
            self.blocking(scope, "vote tree binding", move || {
                crate::precompute::vote_tree_for(&db, transport)
            })
            .await?
        };
        let witness = {
            let db = Arc::clone(&self.database);
            let witness_round_id = round_id.clone();
            let held_lock = Arc::clone(lock);
            let operation = crate::proving_runtime::Operation::controlled(
                format!("tree:{}:{}", self.database.sidecar_id(), scope.round_id),
                scope.chain().clone(),
                scope.entry_epoch(),
            );
            let _operation_owner = operation.owner();
            self.blocking(scope, "vote tree sync and witness", move || {
                operation.enter(|| {
                    let _held_lock = held_lock;
                    tree.sync_and_witness(&db, &witness_round_id, bundle_index, &node_urls)
                })
            })
            .await
        };
        if scope.interrupted() {
            return self.step_cancelled(scope, ledger);
        }
        let witness = witness?;
        progress.report(RoundStepProgress::TreeSynced {
            height: witness.anchor_height,
        });
        if scope.interrupted() {
            return self.step_cancelled(scope, ledger);
        }

        // Blocking orchestration submits CPU jobs to the shared pool; stages stream back.
        let (stage_tx, mut stage_rx) = tokio::sync::mpsc::unbounded_channel::<VoteCommitStage>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let db = Arc::clone(&self.database);
        let max_proof_concurrency = host.max_proof_concurrency.max(1);
        // The prover keeps the round lock until persistence has finished,
        // even if this future is dropped, so a new pass cannot observe the
        // old CastVote plan and start a competing proof meanwhile.
        let held_lock = Arc::clone(lock);
        let proving_round_id = round_id.clone();
        let observations = scope.observations.clone();
        let operation = crate::proving_runtime::Operation::controlled(
            format!(
                "{}:{}:{}:{}",
                self.database.sidecar_id(),
                scope.wallet_id,
                scope.round_id,
                bundle_index
            ),
            scope.chain().clone(),
            scope.entry_epoch(),
        );
        let _operation_owner = operation.owner();
        tokio::task::spawn_blocking(move || {
            operation.enter(|| {
                let _held_lock = held_lock;
                let result = (|| {
                    let hotkey =
                        VotingHotkey::from_stored_secret(hotkey_secret.as_slice(), network)?;
                    let stages = VoteCommitStageBridge::new(move |stage| {
                        let _ = stage_tx.send(stage);
                    });
                    crate::proving_runtime::check_interruption()?;
                    let prepared = crate::vote::observe_prepare_vote_work(
                        &db,
                        VoteSigner::hotkey(&hotkey),
                        VoteWorkRequest {
                            round_id: &proving_round_id,
                            bundle_index,
                            drafts: &drafts,
                            witness: &witness,
                            stages: &stages,
                            max_proof_concurrency,
                        },
                        &observations,
                    )?;
                    crate::vote::observe_persist_prepared_vote_work(&db, prepared, &observations)
                })();
                let _ = done_tx.send(result);
            })
        });
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
                            Some(&scope.step),
                            None,
                            &ledger,
                            "vote commit thread exited without a result",
                        )
                    })?;
                }
            }
        };
        while let Ok(stage) = stage_rx.try_recv() {
            progress.report(RoundStepProgress::VoteCommit(stage));
        }
        if scope.interrupted() {
            return self.step_cancelled(scope, ledger);
        }
        let recovery = recovery
            .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;

        let (votes, batch) = self
            .recover_committed(&round_id, recovery, &scope.observations)
            .map_err(|error| self.step_voting_failure(error, Some(&scope.step), &ledger))?;
        self.complete_vote_unit(
            scope,
            votes,
            batch,
            CompletionEntry::FreshCast,
            &host.chain_policy,
            ledger,
            progress,
        )
        .await
    }

    pub(super) fn recover_committed(
        &self,
        round_id: &str,
        recovery: VoteCommitmentRecovery,
        observations: &crate::ObservationScope,
    ) -> Result<(Vec<CommittedVote>, Option<SignedVoteBatch>), VotingError> {
        let bundle_index = recovery.bundle_index();
        let votes = recovery
            .proposal_ids()
            .into_iter()
            .map(|proposal_id| {
                CommittedVote::observe_recover(
                    &self.database,
                    round_id,
                    bundle_index,
                    proposal_id,
                    observations,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let batch = match recovery {
            VoteCommitmentRecovery::AtomicBatch(batch) => Some(batch),
            _ => None,
        };
        Ok((votes, batch))
    }
}

/// Validates the complete vote-tree node list before any sync and returns the
/// base URLs the tree client will extend.
///
/// Every URL must parse with an `http` or `https` scheme and a host, carry no
/// query or fragment, and on Mainnet use HTTPS, matching the chain client's
/// endpoint rule: a plaintext tree endpoint would expose the wallet's
/// round-specific traffic and let an on-path attacker serve a forged tree that
/// drives expensive proving before the chain rejects the anchor. The tree
/// client appends its API path to the base verbatim, so a query would swallow
/// the path and a trailing slash would double it; trailing slashes are
/// removed and query or fragment forms rejected.
pub(super) fn canonical_vote_tree_node_urls(
    node_urls: &[String],
    network: Network,
) -> Result<Vec<String>, VotingError> {
    if node_urls.is_empty() {
        return Err(VotingError::InvalidInput {
            message: "casting a vote requires at least one vote-tree node URL".to_string(),
        });
    }
    node_urls
        .iter()
        .map(|node_url| {
            if node_url.contains('?') || node_url.contains('#') {
                return Err(VotingError::InvalidInput {
                    message: format!(
                        "vote-tree node URL {node_url:?} must be a base URL without a query or fragment"
                    ),
                });
            }
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
            Ok(node_url.trim_end_matches('/').to_string())
        })
        .collect()
}
