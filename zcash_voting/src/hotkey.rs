use blake2b_simd::Params;
use orchard::keys::FullViewingKey;
use rand::RngCore;
use zeroize::Zeroizing;
use zip32::Scope;

use crate::types::{Network, VotingError, VotingHotkey};

const HOTKEY_CONTEXT_PREFIX: &[u8] = b"ZcashVotingHotkeyV1";
const HOTKEY_SEED_PERSONALIZATION: &[u8] = b"ZcashVotingHotKy";
const HOTKEY_SEED_LEN: usize = 64;

/// ZIP-32 account index used for voting hotkey signing keys.
pub const VOTING_HOTKEY_ACCOUNT_INDEX: u32 = 0;

/// Orchard address index used for the delegation output target.
pub const VOTING_HOTKEY_ADDRESS_INDEX: u32 = 0;

/// Context that binds a software wallet seed to one voting hotkey.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotkeyDerivationContext<'a> {
    pub round_id: &'a str,
    pub account_id: &'a str,
}

/// Derives the canonical voting hotkey for one wallet account in one round.
///
/// The wallet seed is length-prefixed together with `round_id` and `account_id`
/// plus a network tag before being hashed into dedicated hotkey seed material.
/// The resulting hotkey is used as the delegation output target and as the vote
/// signer.
///
/// # Errors
///
/// Returns [`VotingError::InvalidInput`] when the wallet seed is too short, a
/// context field cannot be length-prefixed, or the derived seed cannot produce
/// an Orchard key for `network`.
pub fn derive_voting_hotkey(
    wallet_seed: &[u8],
    context: HotkeyDerivationContext<'_>,
    network: Network,
) -> Result<VotingHotkey, VotingError> {
    let hotkey_seed = derive_contextual_hotkey_seed(wallet_seed, context, network)?;
    voting_hotkey_from_seed(&hotkey_seed, network)
}

/// Generates a random app-owned voting hotkey.
///
/// Hardware wallets do not expose the wallet seed to the host app. For those
/// flows, the host app should generate this secret once, store
/// [`VotingHotkey::secret_seed`] in platform secure storage, and reconstruct it
/// later with [`voting_hotkey_from_seed`].
///
/// # Errors
///
/// Returns [`VotingError::InvalidInput`] when the generated seed cannot produce
/// an Orchard key for `network`.
pub fn generate_random_voting_hotkey(network: Network) -> Result<VotingHotkey, VotingError> {
    let mut seed = Zeroizing::new(vec![0u8; HOTKEY_SEED_LEN]);
    rand::rngs::OsRng.fill_bytes(seed.as_mut_slice());
    voting_hotkey_from_seed(&seed, network)
}

/// Reconstructs a voting hotkey from previously stored hotkey seed bytes.
///
/// # Errors
///
/// Returns [`VotingError::InvalidInput`] when `seed` is too short or cannot
/// produce an Orchard key for `network`.
pub fn voting_hotkey_from_seed(seed: &[u8], network: Network) -> Result<VotingHotkey, VotingError> {
    if seed.len() < 32 {
        return Err(VotingError::InvalidInput {
            message: format!("seed must be at least 32 bytes, got {}", seed.len()),
        });
    }

    let raw_orchard_address = raw_orchard_address_from_seed(
        seed,
        network,
        VOTING_HOTKEY_ACCOUNT_INDEX,
        VOTING_HOTKEY_ADDRESS_INDEX,
    )?;

    Ok(VotingHotkey::from_parts(
        seed.to_vec(),
        raw_orchard_address,
        VOTING_HOTKEY_ADDRESS_INDEX,
        network,
    ))
}

fn derive_contextual_hotkey_seed(
    wallet_seed: &[u8],
    context: HotkeyDerivationContext<'_>,
    network: Network,
) -> Result<Zeroizing<Vec<u8>>, VotingError> {
    if wallet_seed.len() < 32 {
        return Err(VotingError::InvalidInput {
            message: format!(
                "wallet_seed must be at least 32 bytes, got {}",
                wallet_seed.len()
            ),
        });
    }

    let mut material = Zeroizing::new(Vec::new());
    material.extend_from_slice(HOTKEY_CONTEXT_PREFIX);
    append_context_part(&mut material, wallet_seed)?;
    append_context_part(&mut material, context.round_id.as_bytes())?;
    append_context_part(&mut material, context.account_id.as_bytes())?;
    append_context_part(&mut material, network_tag(network))?;

    let hash = Params::new()
        .hash_length(HOTKEY_SEED_LEN)
        .personal(HOTKEY_SEED_PERSONALIZATION)
        .hash(&material);

    Ok(Zeroizing::new(hash.as_bytes().to_vec()))
}

fn network_tag(network: Network) -> &'static [u8] {
    match network {
        Network::Mainnet => b"mainnet",
        Network::Testnet => b"testnet",
        Network::Regtest => b"regtest",
    }
}

fn raw_orchard_address_from_seed(
    seed: &[u8],
    network: Network,
    account_index: u32,
    address_index: u32,
) -> Result<[u8; 43], VotingError> {
    let spending_key = network.orchard_spending_key_from_seed(seed, account_index)?;
    let full_viewing_key = FullViewingKey::from(&spending_key);
    let address = full_viewing_key.address_at(u64::from(address_index), Scope::External);

    Ok(address.to_raw_address_bytes())
}

fn append_context_part(material: &mut Vec<u8>, part: &[u8]) -> Result<(), VotingError> {
    let len = u32::try_from(part.len()).map_err(|_| VotingError::InvalidInput {
        message: "voting hotkey context part length exceeds u32::MAX".to_string(),
    })?;
    material.extend_from_slice(&len.to_be_bytes());
    material.extend_from_slice(part);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const OTHER_ACCOUNT_ID: &str = "550e8400-e29b-41d4-a716-446655440001";
    const ROUND_ID: &str = "round-1";
    const OTHER_ROUND_ID: &str = "round-2";

    fn context<'a>(round_id: &'a str, account_id: &'a str) -> HotkeyDerivationContext<'a> {
        HotkeyDerivationContext {
            round_id,
            account_id,
        }
    }

    #[test]
    fn contextual_hotkey_is_deterministic() {
        let seed = [0xAB; 64];
        let first =
            derive_voting_hotkey(&seed, context(ROUND_ID, ACCOUNT_ID), Network::Regtest).unwrap();
        let second =
            derive_voting_hotkey(&seed, context(ROUND_ID, ACCOUNT_ID), Network::Regtest).unwrap();

        assert_eq!(first.secret_seed(), second.secret_seed());
        assert_eq!(first.raw_orchard_address(), second.raw_orchard_address());
        assert_eq!(first.address_index(), VOTING_HOTKEY_ADDRESS_INDEX);
        assert_eq!(first.network(), Network::Regtest);
    }

    #[test]
    fn contextual_hotkey_is_bound_to_round_and_account() {
        let seed = [0xAB; 64];
        let base =
            derive_voting_hotkey(&seed, context(ROUND_ID, ACCOUNT_ID), Network::Regtest).unwrap();
        let other_round =
            derive_voting_hotkey(&seed, context(OTHER_ROUND_ID, ACCOUNT_ID), Network::Regtest)
                .unwrap();
        let other_account =
            derive_voting_hotkey(&seed, context(ROUND_ID, OTHER_ACCOUNT_ID), Network::Regtest)
                .unwrap();

        assert_ne!(base.secret_seed(), other_round.secret_seed());
        assert_ne!(base.secret_seed(), other_account.secret_seed());
    }

    #[test]
    fn contextual_hotkey_has_valid_raw_orchard_address() {
        let seed = [0xAB; 64];
        let hotkey =
            derive_voting_hotkey(&seed, context(ROUND_ID, ACCOUNT_ID), Network::Regtest).unwrap();

        assert_eq!(hotkey.raw_orchard_address().len(), 43);
    }

    #[test]
    fn contextual_hotkey_is_bound_to_network() {
        let seed = [0xAB; 64];
        let testnet =
            derive_voting_hotkey(&seed, context(ROUND_ID, ACCOUNT_ID), Network::Testnet).unwrap();
        let mainnet =
            derive_voting_hotkey(&seed, context(ROUND_ID, ACCOUNT_ID), Network::Mainnet).unwrap();

        assert_ne!(testnet.secret_seed(), mainnet.secret_seed());
        assert_ne!(testnet.raw_orchard_address(), mainnet.raw_orchard_address());
    }

    #[test]
    fn random_hotkey_returns_storable_secret_seed() {
        let first = generate_random_voting_hotkey(Network::Regtest).unwrap();
        let second = generate_random_voting_hotkey(Network::Regtest).unwrap();

        assert_eq!(first.secret_seed().len(), HOTKEY_SEED_LEN);
        assert_eq!(second.secret_seed().len(), HOTKEY_SEED_LEN);
        assert_ne!(first.secret_seed(), second.secret_seed());
    }

    #[test]
    fn stored_hotkey_seed_reconstructs_address() {
        let hotkey = generate_random_voting_hotkey(Network::Regtest).unwrap();
        let reconstructed =
            voting_hotkey_from_seed(hotkey.secret_seed(), Network::Regtest).unwrap();

        assert_eq!(
            hotkey.raw_orchard_address(),
            reconstructed.raw_orchard_address()
        );
    }

    #[test]
    fn hotkey_feeds_typed_delegation_and_vote_signer_paths() {
        use zcash_protocol::consensus::{NetworkConstants, Parameters};

        let seed = [0xAB; 64];
        let hotkey =
            derive_voting_hotkey(&seed, context(ROUND_ID, ACCOUNT_ID), Network::Regtest).unwrap();
        let keys = crate::delegate::DelegationKeys::with_voting_hotkey(
            vec![8; 96],
            &hotkey,
            [9; 32],
            0,
            "Demo Round".to_string(),
        )
        .unwrap();

        assert_eq!(&keys.hotkey_raw_address, hotkey.raw_orchard_address());
        assert_eq!(keys.address_index, hotkey.address_index());
        assert_eq!(keys.coin_type, Network::Regtest.network_type().coin_type());

        match crate::vote::VoteSigner::hotkey(&hotkey) {
            crate::vote::VoteSigner::Hotkey {
                hotkey: signer_hotkey,
            } => {
                assert_eq!(signer_hotkey.secret_seed(), hotkey.secret_seed());
                assert_eq!(signer_hotkey.network(), hotkey.network());
            }
            crate::vote::VoteSigner::HotkeySeed { .. } => panic!("expected typed hotkey signer"),
        }
    }

    #[test]
    fn orchard_spending_key_uses_zip32_account_index() {
        let seed = [0x42; 64];

        let default = Network::Regtest
            .orchard_spending_key_from_seed(&seed, VOTING_HOTKEY_ACCOUNT_INDEX)
            .unwrap();
        let account_0 = Network::Regtest
            .orchard_spending_key_from_seed(&seed, 0)
            .unwrap();
        let account_1 = Network::Regtest
            .orchard_spending_key_from_seed(&seed, 1)
            .unwrap();

        assert_eq!(
            FullViewingKey::from(&default).to_bytes(),
            FullViewingKey::from(&account_0).to_bytes()
        );
        assert_ne!(
            FullViewingKey::from(&account_0).to_bytes(),
            FullViewingKey::from(&account_1).to_bytes()
        );
    }

    #[test]
    fn short_wallet_seed_is_rejected() {
        let err =
            derive_voting_hotkey(&[0x01; 16], context(ROUND_ID, ACCOUNT_ID), Network::Regtest)
                .unwrap_err()
                .to_string();

        assert!(err.contains("wallet_seed must be at least 32 bytes"));
    }
}
