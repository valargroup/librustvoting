use super::*;
use crate::chain_submission::{ChainSubmissionIdentity, ChainSubmissionTarget};
use crate::delegate_and_vote_batch::DelegationAuthorization;

fn authorization(wallet: &str, round: [u8; 32], bundle: u32) -> DelegationAuthorization {
    // All payload bytes deliberately stay identical across scopes. Sharing
    // cryptographic material does not make storage identities interchangeable.
    DelegationAuthorization {
        identity: ChainSubmissionIdentity::new(
            wallet,
            Network::Testnet,
            round,
            bundle,
            ChainSubmissionTarget::Delegation,
        )
        .unwrap(),
        generation_digest: [7; 32],
        signature: [8; 64],
        van: [9; 32],
        submission: crate::wire::DelegationSubmissionWire {
            rk: String::new(),
            spend_auth_sig: String::new(),
            tx1_effects: String::new(),
            nf_signed: String::new(),
            cmx_new: String::new(),
            gov_comm: String::new(),
            gov_nullifiers: Vec::new(),
            proof: String::new(),
            vote_round_id: String::new(),
        },
    }
}

#[test]
fn combined_authorization_requires_the_same_wallet_round_and_bundle() {
    let authorization = authorization(WALLET_ID, [1; 32], 0);
    authorization
        .validate_scope(WALLET_ID, ROUND_ID, 0)
        .unwrap();
    for (wallet, round, bundle) in [
        ("other-wallet", ROUND_ID.to_owned(), 0),
        (WALLET_ID, hex::encode([2; 32]), 0),
        (WALLET_ID, ROUND_ID.to_owned(), 1),
    ] {
        assert!(authorization
            .validate_scope(wallet, &round, bundle)
            .is_err());
    }
}

#[test]
fn combined_persistence_rejects_mixed_storage_scopes_before_writing() {
    for (wallet, round, bundle) in [
        ("other-wallet", [1; 32], 0),
        (WALLET_ID, [2; 32], 0),
        (WALLET_ID, [1; 32], 1),
    ] {
        let db = db_with_vote();
        let mut prepared = prepared_atomic_vote_batch_fixture(&db);
        prepared.delegation = Some(authorization(wallet, round, bundle));
        let before: Vec<(u32, Option<String>)> = {
            let conn = db.conn();
            let mut query = conn
                .prepare(
                    "SELECT proposal_id, commitment_bundle_json FROM votes ORDER BY proposal_id",
                )
                .unwrap();
            query
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let error = persist_prepared_atomic_vote_batch(&db, prepared).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("authorization does not match the vote batch storage identity"),
            "{error}"
        );
        let conn = db.conn();
        let mut query = conn
            .prepare("SELECT proposal_id, commitment_bundle_json FROM votes ORDER BY proposal_id")
            .unwrap();
        let after: Vec<(u32, Option<String>)> = query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(before, after);
        let authorizations: u32 = conn
            .query_row("SELECT count(*) FROM delegate_cast_recovery", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(authorizations, 0);
    }
}
