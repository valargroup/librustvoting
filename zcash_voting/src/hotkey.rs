use orchard::keys::{FullViewingKey, SpendingKey};
use rand::RngCore;
use zcash_keys::keys::UnifiedSpendingKey;
use zeroize::Zeroizing;
use zip32::{AccountId, Scope};

use crate::types::{Network, VotingError, VotingHotkey};

const HOTKEY_SEED_LEN: usize = 64;

/// ZIP-32 account index used for voting hotkey signing keys.
pub const VOTING_HOTKEY_ACCOUNT_INDEX: u32 = 0;

/// Orchard address index used for the delegation output target.
pub const VOTING_HOTKEY_ADDRESS_INDEX: u32 = 0;

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
/// Wallets that start with a root seed should derive scoped voting hotkey seed
/// material at the wallet boundary, then pass only that scoped seed to this
/// function when reconstruction is needed.
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

pub(crate) fn spending_key_from_hotkey_seed(
    seed: &[u8],
    network: Network,
    account_index: u32,
) -> Result<SpendingKey, VotingError> {
    if seed.len() < 32 {
        return Err(VotingError::InvalidInput {
            message: format!("seed must be at least 32 bytes, got {}", seed.len()),
        });
    }

    let account = AccountId::try_from(account_index).map_err(|_| VotingError::InvalidInput {
        message: format!("invalid account_index {account_index}"),
    })?;

    let usk = UnifiedSpendingKey::from_seed(&network, seed, account).map_err(|e| {
        VotingError::InvalidInput {
            message: format!("failed to derive UnifiedSpendingKey from seed: {e}"),
        }
    })?;

    Ok(*usk.orchard())
}

fn raw_orchard_address_from_seed(
    seed: &[u8],
    network: Network,
    account_index: u32,
    address_index: u32,
) -> Result<[u8; 43], VotingError> {
    let spending_key = spending_key_from_hotkey_seed(seed, network, account_index)?;
    let full_viewing_key = FullViewingKey::from(&spending_key);
    let address = full_viewing_key.address_at(u64::from(address_index), Scope::External);

    Ok(address.to_raw_address_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_hotkey_seed_reconstructs_address() {
        let hotkey = voting_hotkey_from_seed(&[0xAB; 64], Network::Regtest).unwrap();
        let reconstructed =
            voting_hotkey_from_seed(hotkey.secret_seed(), Network::Regtest).unwrap();

        assert_eq!(hotkey.secret_seed(), reconstructed.secret_seed());
        assert_eq!(
            hotkey.raw_orchard_address(),
            reconstructed.raw_orchard_address()
        );
        assert_eq!(hotkey.address_index(), VOTING_HOTKEY_ADDRESS_INDEX);
        assert_eq!(hotkey.network(), Network::Regtest);
    }

    #[test]
    fn hotkey_seed_is_bound_to_network() {
        let testnet = voting_hotkey_from_seed(&[0xAB; 64], Network::Testnet).unwrap();
        let mainnet = voting_hotkey_from_seed(&[0xAB; 64], Network::Mainnet).unwrap();

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
    fn hotkey_feeds_typed_delegation_and_vote_signer_paths() {
        use zcash_protocol::consensus::{NetworkConstants, Parameters};

        let hotkey = voting_hotkey_from_seed(&[0xAB; 64], Network::Regtest).unwrap();
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
    fn hotkey_spending_key_uses_zip32_account_index() {
        let seed = [0x42; 64];

        let default =
            spending_key_from_hotkey_seed(&seed, Network::Regtest, VOTING_HOTKEY_ACCOUNT_INDEX)
                .unwrap();
        let account_0 = spending_key_from_hotkey_seed(&seed, Network::Regtest, 0).unwrap();
        let account_1 = spending_key_from_hotkey_seed(&seed, Network::Regtest, 1).unwrap();

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
    fn short_hotkey_seed_is_rejected() {
        let err = voting_hotkey_from_seed(&[0x01; 16], Network::Regtest)
            .unwrap_err()
            .to_string();

        assert!(err.contains("seed must be at least 32 bytes"));
    }
}
