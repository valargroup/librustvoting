//! Fixtures shared by the broadcast-protection and setup-rebuild groups.

use super::*;

/// Seeds one round with `bundle_count` bundles that carry full delegation
/// setup, so every recovery case below starts from a bundle worth losing.
pub(super) fn db_with_delegation_setup(bundle_count: u32) -> VotingDb {
    let db = test_db();
    db.init_round(Network::Testnet, &test_params(), None)
        .unwrap();
    let conn = db.conn();
    for bundle_index in 0..bundle_count {
        queries::insert_bundle(&conn, ROUND_ID, W, bundle_index, &[1]).unwrap();
        queries::store_delegation_data(
            &conn,
            ROUND_ID,
            W,
            bundle_index,
            &[0x11; 32],
            &[vec![0x22; 32]],
            &[0x33; 32],
            &[vec![0x44; 32]],
            &[0x55; 32],
            &[0x66; 32],
            &[0x77; 32],
            &[0x88; 32],
            &[0x99; 32],
            &[0xAA; 32],
            1_000,
            0,
            &[(vec![0xBB; 32], vec![0xCC; 32])],
            &[0xDD; 32],
            &crate::tx1::placeholder_tx1_effects(),
        )
        .unwrap();
    }
    drop(conn);
    db
}

pub(super) fn van_comm_rand_of(db: &VotingDb, bundle_index: u32) -> Option<Vec<u8>> {
    db.conn()
        .query_row(
            "SELECT van_comm_rand FROM bundles
              WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = ?3",
            rusqlite::params![ROUND_ID, W, bundle_index],
            |row| row.get::<_, Option<Vec<u8>>>(0),
        )
        .unwrap()
}

pub(super) fn insert_chain_submission(db: &VotingDb, bundle_index: u32) {
    db.conn()
        .execute(
            "INSERT INTO chain_submissions
             (identity_key, round_id, wallet_id, network, bundle_index, kind,
              generation_digest, state, committed_post_reservations,
              created_at, updated_at)
             VALUES (?1, ?2, ?3, 'testnet', ?4, 'delegation', ?5,
                     'submitting', 0, 1, 1)",
            rusqlite::params![
                vec![bundle_index as u8 + 1; 32],
                ROUND_ID,
                W,
                bundle_index,
                vec![0x42_u8; 32],
            ],
        )
        .unwrap();
}

pub(super) fn insert_vote_row(db: &VotingDb, bundle_index: u32) {
    db.conn()
        .execute(
            "INSERT INTO votes
             (round_id, wallet_id, bundle_index, proposal_id, choice, created_at)
             VALUES (?1, ?2, ?3, 1, 1, 1)",
            rusqlite::params![ROUND_ID, W, bundle_index],
        )
        .unwrap();
}

/// One Ironwood note plus a round seeded for it, the shape every real
/// delegation setup needs.
pub(super) fn ironwood_setup_fixture() -> (VotingDb, NoteInfo, Vec<u8>) {
    use orchard::{
        note::{NoteVersion, Rho},
        value::NoteValue,
    };
    use voting_crypto_deps::rand::rngs::OsRng;
    use zcash_keys::keys::UnifiedSpendingKey;
    use zip32::{AccountId, Scope};

    let seed = [0x42u8; 32];
    let account = AccountId::try_from(0u32).unwrap();
    let usk = UnifiedSpendingKey::from_seed(&Network::Regtest, &seed, account).unwrap();
    let ufvk = usk.to_unified_full_viewing_key();
    let fvk = ufvk.orchard().unwrap().clone();
    let address = fvk.address_at(0u32, Scope::External);

    let mut rng = OsRng;
    let (_, _, parent_note) = orchard::Note::dummy(&mut rng, None, NoteVersion::V3);
    let note = orchard::Note::new(
        address,
        NoteValue::from_raw(13_000_000),
        Rho::from_nf_old(parent_note.nullifier(&fvk)),
        NoteVersion::V3,
        &mut rng,
    );
    let note_info =
        NoteInfo::from_orchard_note(&note, 7, Scope::External, &ufvk, &Network::Regtest).unwrap();

    let db = test_db();
    db.init_round(Network::Regtest, &test_params_nu6_3(), None)
        .unwrap();
    db.ensure_bundles(ROUND_ID, &[note_info.clone()]).unwrap();
    (db, note_info, fvk.to_bytes().to_vec())
}

pub(super) fn keys_for_hotkey_byte(fvk_bytes: &[u8], hotkey_byte: u8) -> DelegationKeys {
    let voting_hotkey =
        VotingHotkey::from_stored_secret(&[hotkey_byte; 64], Network::Regtest).unwrap();
    test_delegation_keys(fvk_bytes.to_vec(), &voting_hotkey, [0x42; 32], 0)
}
