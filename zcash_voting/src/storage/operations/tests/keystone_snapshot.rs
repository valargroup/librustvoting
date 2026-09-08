//! A signing request's validated target and transaction belong to one snapshot.

use super::{fixtures::*, *};

#[test]
fn keystone_snapshot_keeps_the_validated_transaction_during_setup_replacement() {
    let (_, note, fvk_bytes) = ironwood_setup_fixture();
    let path = std::env::temp_dir().join(format!(
        "keystone-signing-snapshot-{}.sqlite",
        uuid::Uuid::new_v4()
    ));
    let reader = VotingDb::open(path.to_str().unwrap()).unwrap();
    reader.set_wallet_id(W);
    reader
        .init_round(Network::Regtest, &test_params_nu6_3(), None)
        .unwrap();
    let notes = [note];
    reader.ensure_bundles(ROUND_ID, &notes).unwrap();
    let first_keys = keys_for_hotkey_byte(&fvk_bytes, 0x43);
    let first = reader
        .build_governance_pczt(ROUND_ID, 0, &notes, &first_keys, nu6_3_branch_id())
        .unwrap();

    let writer = VotingDb::open(path.to_str().unwrap()).unwrap();
    writer.set_wallet_id(W);
    assert!(!reader.shares_connection_with(&writer));
    let second_keys = keys_for_hotkey_byte(&fvk_bytes, 0x44);
    let identity = DelegationProofIdentity::new(reader.sidecar_id(), W.to_string(), ROUND_ID, 0);

    let (before, after, replacement) = reader
        .read_transaction("Keystone signing snapshot", |tx| {
            let before = delegation_pczt::load(tx, &identity, &notes, &first_keys)?;
            // This separate connection commits a different target while the
            // reader still holds its snapshot, as another process could do.
            let replacement = writer.build_governance_pczt(
                ROUND_ID,
                0,
                &notes,
                &second_keys,
                nu6_3_branch_id(),
            )?;
            let after = delegation_pczt::load(tx, &identity, &notes, &first_keys)?;
            Ok((before, after, replacement))
        })
        .unwrap();

    assert_eq!(before, after);
    assert_eq!(after.0, first.pczt_bytes);
    assert_eq!(after.1, first.pczt_sighash);
    assert_eq!(after.2, first.rk);
    assert_ne!(after.0, replacement.pczt_bytes);
    assert!(matches!(
        reader.validated_delegation_pczt(&identity, &notes, &first_keys),
        Err(VotingError::DelegationTargetMismatch { .. })
    ));
    let current = reader
        .validated_delegation_pczt(&identity, &notes, &second_keys)
        .unwrap();
    assert_eq!(current.0, replacement.pczt_bytes);
    assert_eq!(current.1, replacement.pczt_sighash);
    assert_eq!(current.2, replacement.rk);
    drop(writer);
    drop(reader);
    std::fs::remove_file(path).unwrap();
}
