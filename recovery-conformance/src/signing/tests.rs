use super::*;

#[test]
fn voting_hotkey_is_stable_for_one_account_and_round() {
    let seed = [0x42; 64];
    let first = voting_hotkey(&seed, "account", "round", Network::Testnet).unwrap();
    let restored = voting_hotkey(&seed, "account", "round", Network::Testnet).unwrap();

    assert_eq!(first.stored_secret(), restored.stored_secret());
    assert_eq!(first.delegation_target(), restored.delegation_target());
}

#[test]
fn voting_hotkey_is_domain_separated_by_account_and_round() {
    let seed = [0x42; 64];
    let baseline = voting_hotkey(&seed, "account-a", "round-a", Network::Testnet).unwrap();
    let other_account = voting_hotkey(&seed, "account-b", "round-a", Network::Testnet).unwrap();
    let other_round = voting_hotkey(&seed, "account-a", "round-b", Network::Testnet).unwrap();

    assert_ne!(baseline.stored_secret(), other_account.stored_secret());
    assert_ne!(baseline.stored_secret(), other_round.stored_secret());
}
