//! `DelegateAndVoteBatchWire::authorization_digest` is the envelope's own
//! validation: it refuses every shape the chain's combined endpoint would.

use crate::wire::{
    DelegateAndVoteBatchWire, DelegationSubmissionWire, VoteCommitmentBatchWire, VoteCommitmentWire,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};

fn field(byte: u8) -> String {
    STANDARD.encode([byte; 32])
}

fn vote(proposal_id: u32) -> VoteCommitmentWire {
    VoteCommitmentWire {
        van_nullifier: field(0x10),
        vote_authority_note_new: field(proposal_id as u8),
        vote_commitment: field(0x20 + proposal_id as u8),
        proposal_id,
        proof: STANDARD.encode([0x13; 96]),
        vote_round_id: field(0x11),
        anchor_height: 0,
        r_vpk: field(0x15),
        vote_auth_sig: STANDARD.encode([0x17; 64]),
    }
}

fn envelope(count: u32) -> DelegateAndVoteBatchWire {
    DelegateAndVoteBatchWire {
        delegation: DelegationSubmissionWire {
            rk: field(0x01),
            spend_auth_sig: STANDARD.encode([0x02; 64]),
            tx1_effects: field(0x03),
            nf_signed: field(0x04),
            cmx_new: field(0x05),
            gov_comm: field(0x09),
            gov_nullifiers: vec![field(0x06)],
            proof: STANDARD.encode([0x07; 96]),
            vote_round_id: field(0x11),
        },
        batch: VoteCommitmentBatchWire {
            votes: (1..=count).map(vote).collect(),
        },
    }
}

#[test]
fn a_well_formed_envelope_digests_and_serializes() {
    let envelope = envelope(2);
    let digest = envelope.authorization_digest().unwrap();
    let effects: Vec<([u8; 32], [u8; 32])> = [1u8, 2]
        .into_iter()
        .map(|proposal| ([proposal; 32], [0x20 + proposal; 32]))
        .collect();
    let actions = effects
        .iter()
        .zip([1u32, 2])
        .map(|((successor, commitment), proposal_id)| {
            crate::vote_commitment::CastVoteBatchSighashAction {
                r_vpk: &[0x15; 32],
                van_nullifier: &[0x10; 32],
                vote_authority_note_new: successor,
                vote_commitment: commitment,
                proposal_id,
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        digest,
        crate::delegate_and_vote_batch::delegate_and_vote_batch_sighash(
            &[0x11; 32],
            &[0x09; 32],
            &actions,
        )
        .unwrap()
    );
    let json = envelope.to_json().unwrap();
    assert_eq!(
        serde_json::from_str::<DelegateAndVoteBatchWire>(&json).unwrap(),
        envelope
    );
}

type Mutation = Box<dyn Fn(&mut DelegateAndVoteBatchWire)>;

#[test]
fn every_malformed_envelope_is_refused_before_serialization() {
    let cases: Vec<(&str, Mutation)> = vec![
        (
            "anchor must be zero",
            Box::new(|envelope| envelope.batch.votes[1].anchor_height = 1),
        ),
        (
            "cast round must match the delegation round",
            Box::new(|envelope| envelope.batch.votes[0].vote_round_id = field(0x12)),
        ),
        (
            "non-canonical base64",
            Box::new(|envelope| {
                let canonical = field(0x15);
                envelope.batch.votes[0].r_vpk = format!("{}=", &canonical[..canonical.len() - 1]);
                // Same bytes with a different final sextet: decodes, but does
                // not re-encode to itself.
                let mut bytes = canonical.into_bytes();
                let last = bytes.len() - 2;
                bytes[last] = b'W';
                envelope.batch.votes[0].r_vpk = String::from_utf8(bytes).unwrap();
            }),
        ),
        (
            "invalid base64",
            Box::new(|envelope| envelope.batch.votes[0].van_nullifier = "*not base64*".into()),
        ),
        (
            "effect must be 32 bytes",
            Box::new(|envelope| envelope.batch.votes[0].vote_commitment = STANDARD.encode([1; 31])),
        ),
        (
            "delegation VAN must be a field element",
            Box::new(|envelope| envelope.delegation.gov_comm = STANDARD.encode([0xff; 32])),
        ),
        (
            "delegation round must be a field element",
            Box::new(|envelope| envelope.delegation.vote_round_id = STANDARD.encode([0xff; 32])),
        ),
        (
            "duplicate proposals",
            Box::new(|envelope| envelope.batch.votes[1].proposal_id = 1),
        ),
        (
            "proposal out of range",
            Box::new(|envelope| envelope.batch.votes[1].proposal_id = 0),
        ),
        (
            "empty batch",
            Box::new(|envelope| envelope.batch.votes.clear()),
        ),
    ];
    for (name, mutate) in cases {
        let mut candidate = envelope(2);
        mutate(&mut candidate);
        assert!(candidate.authorization_digest().is_err(), "{name}");
        assert!(candidate.to_json().is_err(), "{name}");
    }

    let oversized = envelope(crate::vote::MAX_VOTE_BATCH_ACTIONS as u32 + 1);
    assert!(oversized.authorization_digest().is_err());
    assert!(envelope(crate::vote::MAX_VOTE_BATCH_ACTIONS as u32)
        .authorization_digest()
        .is_ok());
}
