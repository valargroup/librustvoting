//! Deriving the voter's keys, without any of them reaching disk or argv.
//!
//! The seed is read from the environment the process inherits and kept only in
//! memory. It is never written to the run configuration, never passed as an
//! argument, and never logged: a mnemonic in a process listing is readable by
//! anything on the machine.

use std::sync::Arc;

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};

use zcash_voting::{Network, SpendAuthSigner, VotingError, VotingHotkey};

/// Environment variable carrying the voter's 24-word mnemonic.
pub const VOTER_SEED_VAR: &str = "VOTE_SDK_VOTER_TEST";

/// Derives the voter's ZIP-32 seed from the inherited environment.
pub fn voter_seed() -> Result<Vec<u8>> {
    let mnemonic = std::env::var(VOTER_SEED_VAR).unwrap_or_default();
    let mnemonic = mnemonic.trim();
    anyhow::ensure!(
        !mnemonic.is_empty(),
        "{VOTER_SEED_VAR} is unset; run under `infisical run --env=staging -- ...`"
    );
    let mut seed = [0u8; 64];
    pbkdf2::pbkdf2::<hmac::Hmac<sha2::Sha512>>(mnemonic.as_bytes(), b"mnemonic", 2048, &mut seed)
        .map_err(|error| anyhow::anyhow!("deriving the seed: {error}"))?;
    Ok(seed.to_vec())
}

/// Reconstructs the harness's stable per-account, per-round voting hotkey.
///
/// Production hosts persist `VotingHotkey::stored_secret` in secure storage.
/// This harness already receives a fixed test-wallet seed at runtime, so it
/// derives an equivalent pseudorandom secret without writing signing material
/// to the sidecar, run configuration, argv, or logs. Every child and retry for
/// the same identity and round therefore restores the same hotkey.
pub fn voting_hotkey(
    voter_seed: &[u8],
    account_uuid: &str,
    round_id: &str,
    network: Network,
) -> Result<VotingHotkey> {
    let mut derivation = Hmac::<sha2::Sha512>::new_from_slice(voter_seed)
        .map_err(|error| anyhow::anyhow!("initializing voting hotkey derivation: {error}"))?;
    derivation.update(b"recovery-conformance.voting-hotkey.v1\0");
    derivation.update(account_uuid.as_bytes());
    derivation.update(&[0]);
    derivation.update(round_id.as_bytes());
    let secret = zeroize::Zeroizing::new(derivation.finalize().into_bytes().to_vec());
    VotingHotkey::from_stored_secret(&secret, network).map_err(|error| anyhow::anyhow!("{error:?}"))
}

/// A software SpendAuth signer over the voter's seed.
///
/// Randomises the account's spend-authorizing key by the request's `alpha` and
/// signs the sighash. The request's seed fingerprint is checked first: a wallet
/// holding several seeds must route to the right one, and here it catches a
/// sidecar built from a different wallet than the one being signed for, which
/// otherwise fails much later as a rejected authorization.
pub fn software_signer(seed: Vec<u8>) -> Arc<dyn SpendAuthSigner> {
    Arc::new(
        move |request: zcash_voting::delegate::DelegationSigningRequest| {
            let fingerprint =
                zip32::fingerprint::SeedFingerprint::from_seed(&seed).ok_or_else(|| {
                    VotingError::InvalidInput {
                        message: "seed length is not valid for ZIP-32".to_string(),
                    }
                })?;
            if fingerprint.to_bytes() != request.seed_fingerprint {
                return Err(VotingError::InvalidInput {
                    message: "seed does not match the delegation signing request".to_string(),
                });
            }
            let account = zip32::AccountId::try_from(request.account_index).map_err(|_| {
                VotingError::InvalidInput {
                    message: format!("invalid account index {}", request.account_index),
                }
            })?;
            let usk = zcash_voting::backend::zcash_keys::keys::UnifiedSpendingKey::from_seed(
                &request.network,
                &seed,
                account,
            )
            .map_err(|error| VotingError::InvalidInput {
                message: format!("deriving the spending key: {error:?}"),
            })?;
            let ask =
                zcash_voting::backend::orchard::keys::SpendAuthorizingKey::from(usk.orchard());
            let alpha = Option::<voting_crypto_deps::pasta_curves::pallas::Scalar>::from(
            <voting_crypto_deps::pasta_curves::pallas::Scalar as voting_crypto_deps::pasta_curves::group::ff::PrimeField>::from_repr(request.alpha),
        )
        .ok_or_else(|| VotingError::InvalidInput {
            message: "alpha is not a valid Pallas scalar".to_string(),
        })?;
            let signature = ask
                .randomize(&alpha)
                .sign(voting_crypto_deps::rand::rngs::OsRng, &request.sighash);
            Ok((&signature).into())
        },
    )
}

/// Confirms the wallet database belongs to the voter seed.
///
/// Matching a wallet to a seed by note count or balance is unsound: two wallets
/// on the same faucet can hold identical amounts, and one such pair already
/// exists on the development host. Identity is the viewing key.
pub fn wallet_matches_seed(wallet_db: &std::path::Path, seed: &[u8]) -> Result<bool> {
    let connection = rusqlite::Connection::open(wallet_db).context("opening the wallet")?;
    let stored: String = connection
        .query_row("select ufvk from accounts limit 1", [], |row| row.get(0))
        .context("reading the wallet's viewing key")?;
    let derived = zcash_voting::backend::zcash_keys::keys::UnifiedSpendingKey::from_seed(
        &crate::environment::ZCASH_NETWORK,
        seed,
        zip32::AccountId::ZERO,
    )
    .map_err(|error| anyhow::anyhow!("deriving the viewing key: {error:?}"))?
    .to_unified_full_viewing_key()
    .encode(&crate::environment::ZCASH_NETWORK);
    Ok(stored == derived)
}

#[cfg(test)]
#[path = "signing/tests.rs"]
mod tests;
