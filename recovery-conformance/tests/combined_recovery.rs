//! Negative oracles: incomplete or unrelated durable evidence must never pass.
use recovery_conformance::combined::{CombinedBundle, CombinedMember};

fn confirmed(proposals: &[u32]) -> CombinedBundle {
    CombinedBundle {
        wallet_id: "wallet".into(),
        round_id: "round".into(),
        bundle_index: 0,
        pczt_fingerprint: Some("pczt".into()),
        proof_fingerprint: Some("proof".into()),
        van_position: Some(100),
        delegation_hash: Some("hash".into()),
        authorizations: vec![(vec![1; 32], "authorization".into())],
        members: proposals
            .iter()
            .enumerate()
            .map(|(index, proposal)| CombinedMember {
                proposal_id: *proposal,
                batch_digest: Some(vec![1; 32]),
                batch_index: Some(index as u32),
                batch_size: Some(proposals.len() as u32),
                combined: true,
                anchor_height: 0,
                position: Some(101 + index as u64),
                tx_hash: Some("hash".into()),
                has_plan: true,
            })
            .collect(),
    }
}

#[test]
fn singleton_and_multi_proposal_combined_confirmations_are_complete() {
    for proposals in [vec![1], vec![1, 2, 3]] {
        confirmed(&proposals).assert_confirmed(&proposals).unwrap();
    }
}

#[test]
fn every_member_and_its_authorization_are_required() {
    let proposals = [1, 2, 3];
    let valid = confirmed(&proposals);
    let mut missing = valid.clone();
    missing.members.pop();
    assert!(missing.assert_complete(&proposals).is_err());
    let mut missing = valid.clone();
    missing.authorizations.clear();
    assert!(missing.assert_complete(&proposals).is_err());
    let mut wrong = valid.clone();
    wrong.members[1].batch_digest = Some(vec![2; 32]);
    assert!(wrong.assert_complete(&proposals).is_err());
    let mut wrong = valid.clone();
    wrong.members[1].batch_index = Some(0);
    assert!(wrong.assert_complete(&proposals).is_err());
    let mut wrong = valid;
    wrong.members[1].combined = false;
    assert!(wrong.assert_complete(&proposals).is_err());
}

#[test]
fn confirmation_requires_the_delegation_and_every_vote_together() {
    let proposals = [1, 2, 3];
    let valid = confirmed(&proposals);
    let mut partial = valid.clone();
    partial.van_position = None;
    assert!(partial.assert_confirmed(&proposals).is_err());
    let mut partial = valid.clone();
    partial.members[2].position = None;
    assert!(partial.assert_confirmed(&proposals).is_err());
    let mut wrong = valid.clone();
    wrong.members[2].position = Some(104);
    assert!(wrong.assert_confirmed(&proposals).is_err());
    let mut wrong = valid.clone();
    wrong.members[1].tx_hash = Some("other".into());
    assert!(wrong.assert_confirmed(&proposals).is_err());
    let mut missing = valid;
    missing.members[1].has_plan = false;
    assert!(missing.assert_confirmed(&proposals).is_err());
}

#[test]
fn tree_confirmation_does_not_need_a_transaction_hash() {
    let mut tree = confirmed(&[1, 2, 3]);
    tree.delegation_hash = None;
    for member in &mut tree.members {
        member.tx_hash = None;
    }
    tree.assert_confirmed(&[1, 2, 3]).unwrap();
}

#[test]
fn scoped_snapshot_never_borrows_another_wallets_authorization_or_plan() {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    connection.execute_batch("create table bundles(wallet_id text, round_id text, bundle_index integer,
        pczt_sighash blob, van_leaf_position integer, delegation_tx_hash text);
        create table proofs(wallet_id text, round_id text, bundle_index integer, proof blob);
        create table delegate_cast_recovery(wallet_id text, round_id text, bundle_index integer,
            batch_digest blob, delegation_generation_digest blob, spend_auth_signature blob);
        create table votes(wallet_id text, round_id text, bundle_index integer, proposal_id integer,
            commitment_bundle_json text, vc_tree_position integer, tx_hash text);
        create table helper_share_plans(wallet_id text, round_id text, bundle_index integer, proposal_id integer,
            commitment_bundle_json text);
        insert into bundles values('target','round',0,NULL,NULL,NULL);
        insert into bundles values('other','round',0,NULL,NULL,NULL);
        insert into delegate_cast_recovery values('other','round',0,zeroblob(32),zeroblob(32),zeroblob(64));").unwrap();
    let bundles = CombinedBundle::read_all(&connection).unwrap();
    let target = bundles.iter().find(|b| b.wallet_id == "target").unwrap();
    assert!(target.authorizations.is_empty());
    assert!(target.assert_complete(&[1]).is_err());
}

fn snapshot(bundle: CombinedBundle) -> recovery_conformance::assertions::DurableSnapshot {
    recovery_conformance::assertions::DurableSnapshot {
        combined: vec![bundle],
        submissions: Vec::new(),
        bundles: 1,
        proofs: 1,
        votes: 3,
        helper_share_plans: 3,
        share_delegations: 0,
        attempting_urls: 0,
        accepted_urls: 0,
        confirmed_shares: 0,
        pczt_persisted: true,
        cached_tree: false,
        deliveries: Vec::new(),
    }
}

#[test]
fn resumed_authorization_and_prepared_proof_cannot_change() {
    use recovery_conformance::combined::assert_preserved_combined;
    let before = snapshot(confirmed(&[1, 2, 3]));
    assert_preserved_combined(&before, &before).unwrap();
    let mut changed = before.clone();
    changed.combined[0].authorizations[0].1 = "replacement".into();
    assert!(assert_preserved_combined(&before, &changed).is_err());
    let mut changed = before.clone();
    changed.combined[0].proof_fingerprint = Some("new proof".into());
    assert!(assert_preserved_combined(&before, &changed).is_err());
    let mut changed = before.clone();
    changed.combined[0].members[1].batch_digest = Some(vec![2; 32]);
    assert!(assert_preserved_combined(&before, &changed).is_err());
}
