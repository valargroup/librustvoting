//! Snapshot builders for classifier tests. No database: a test states the
//! durable facts directly and reads the obligations back.

use std::collections::BTreeMap;

use crate::chain_submission::planning::LifecycleTransactionHashes;
use crate::phases::{DelegationPhase, DelegationSubmissionStatus, SharePhase, VotePhase};
use crate::round_planning::{BundleSnapshot, RoundSnapshot, VoteSnapshot};
use crate::session::Decision;
use crate::types::{EncryptedShare, ShareDelegationRecord};
use crate::vote::{VoteBatchRecovery, VoteRecoveryBundle};

pub(super) const ROUND: &str = "0101010101010101010101010101010101010101010101010101010101010101";

/// A round snapshot under construction.
pub(super) struct SnapshotBuilder {
    snapshot: RoundSnapshot,
}

pub(super) fn snapshot() -> SnapshotBuilder {
    SnapshotBuilder {
        snapshot: RoundSnapshot {
            round_id: ROUND.to_string(),
            delegations: Vec::new(),
            bundles: BTreeMap::new(),
            votes: BTreeMap::new(),
            shares: Vec::new(),
            share_phases: Vec::new(),
            intents: BTreeMap::new(),
            lifecycle_hashes: LifecycleTransactionHashes::default(),
            persisted_immediate_share: None,
        },
    }
}

impl SnapshotBuilder {
    /// A locally prepared bundle whose delegation is in `phase`.
    pub(super) fn bundle(mut self, bundle_index: u32, phase: DelegationPhase) -> Self {
        self.snapshot.delegations.push(DelegationSubmissionStatus {
            bundle_index,
            phase,
            diagnostic: None,
        });
        self.snapshot.bundles.insert(
            bundle_index,
            BundleSnapshot {
                capability_imported: false,
                delegation_tx_hash: None,
            },
        );
        self
    }

    /// A singleton vote row with recovery material, in `phase`.
    pub(super) fn vote(
        mut self,
        bundle_index: u32,
        proposal_id: u32,
        choice: u32,
        phase: VotePhase,
    ) -> Self {
        let recovery = (phase != VotePhase::Prepared)
            .then(|| recovery_bundle(bundle_index, proposal_id, choice));
        self.snapshot.votes.insert(
            (bundle_index, proposal_id),
            VoteSnapshot {
                choice,
                phase,
                tx_hash: None,
                vc_tree_position: (phase == VotePhase::Confirmed).then_some(7),
                recovery,
            },
        );
        self
    }

    /// An atomic batch of `members` (`(proposal_id, choice)` in order) on
    /// `bundle_index`, every member in `phase`.
    pub(super) fn batch(
        mut self,
        bundle_index: u32,
        members: &[(u32, u32)],
        phase: VotePhase,
    ) -> Self {
        let mut recoveries: Vec<VoteRecoveryBundle> = members
            .iter()
            .enumerate()
            .map(|(index, &(proposal_id, choice))| {
                let mut recovery = recovery_bundle(bundle_index, proposal_id, choice);
                let tag = index as u8 + 0x60;
                recovery.vote_commitment = [tag; 32];
                recovery.van_nullifier = [tag + 0x10; 32];
                recovery.vote_authority_note_new = [tag + 0x20; 32];
                recovery.r_vpk = [tag + 0x30; 32];
                recovery
            })
            .collect();
        let actions = recoveries
            .iter()
            .map(
                |recovery| crate::vote_commitment::CastVoteBatchSighashAction {
                    r_vpk: &recovery.r_vpk,
                    van_nullifier: &recovery.van_nullifier,
                    vote_authority_note_new: &recovery.vote_authority_note_new,
                    vote_commitment: &recovery.vote_commitment,
                    proposal_id: recovery.proposal_id,
                },
            )
            .collect::<Vec<_>>();
        let digest = crate::vote_commitment::cast_vote_batch_sighash(
            ROUND,
            recoveries[0].anchor_height as u64,
            &actions,
        )
        .unwrap();
        let size = recoveries.len() as u32;
        for (index, recovery) in recoveries.iter_mut().enumerate() {
            recovery.batch = Some(VoteBatchRecovery {
                digest,
                index: index as u32,
                size,
            });
        }
        for recovery in recoveries {
            self.snapshot.votes.insert(
                (bundle_index, recovery.proposal_id),
                VoteSnapshot {
                    choice: recovery.vote_decision,
                    phase,
                    tx_hash: None,
                    vc_tree_position: (phase == VotePhase::Confirmed).then_some(7),
                    recovery: Some(recovery),
                },
            );
        }
        self
    }

    /// A helper-share row. `accepted` is whether a helper acknowledged it.
    pub(super) fn share(
        mut self,
        bundle_index: u32,
        proposal_id: u32,
        share_index: u32,
        phase: SharePhase,
        accepted: bool,
    ) -> Self {
        self.snapshot
            .share_phases
            .push((bundle_index, proposal_id, share_index, phase));
        self.snapshot.shares.push(ShareDelegationRecord {
            round_id: ROUND.to_string(),
            bundle_index,
            proposal_id,
            share_index,
            sent_to_urls: if accepted {
                vec!["http://helper.invalid".to_string()]
            } else {
                Vec::new()
            },
            ambiguous_urls: Vec::new(),
            attempting_urls: Vec::new(),
            target_count: 1,
            nullifier: vec![0x77; 32],
            confirmed: phase == SharePhase::Confirmed,
            submit_at: 0,
            created_at: 100,
        });
        self
    }

    pub(super) fn intent(mut self, proposal_id: u32, decision: Decision) -> Self {
        self.snapshot.intents.insert(proposal_id, decision);
        self
    }

    pub(super) fn build(self) -> RoundSnapshot {
        self.snapshot
    }
}

/// Recovery material for one vote with two shares, valid for
/// `share::recover_payloads`.
pub(super) fn recovery_bundle(
    bundle_index: u32,
    proposal_id: u32,
    choice: u32,
) -> VoteRecoveryBundle {
    VoteRecoveryBundle {
        vote_round_id: ROUND.to_string(),
        bundle_index,
        proposal_id,
        vote_decision: choice,
        anchor_height: 123,
        vc_tree_position: 7,
        single_share: false,
        num_options: 3,
        van_nullifier: [0x10; 32],
        vote_authority_note_new: [0x11; 32],
        vote_commitment: [0x12; 32],
        proof: vec![0x13; 96],
        shares_hash: [0x14; 32],
        r_vpk: [0x15; 32],
        alpha_v: [0x16; 32],
        vote_auth_sig: [0x17; 64],
        encrypted_shares: vec![
            EncryptedShare {
                c1: vec![0x21; 32],
                c2: vec![0x22; 32],
                share_index: 0,
                plaintext_value: 5,
                randomness: vec![0x23; 32],
            },
            EncryptedShare {
                c1: vec![0x31; 32],
                c2: vec![0x32; 32],
                share_index: 1,
                plaintext_value: 6,
                randomness: vec![0x33; 32],
            },
        ],
        share_blinds: vec![[0x41; 32], [0x42; 32]],
        share_comms: vec![[0x51; 32], [0x52; 32]],
        batch: None,
    }
}
