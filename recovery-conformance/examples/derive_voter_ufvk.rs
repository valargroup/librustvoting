//! Derives the voter wallet's UFVK from the seed under both BIP39 conventions,
//! so an existing wallet database can be matched with the crate's own key code
//! rather than a hand-rolled fingerprint.
use zcash_voting::backend::zcash_keys::keys::UnifiedSpendingKey;
use zcash_voting::Network;

const WORDLIST: &str = include_str!("bip39-english.txt");

fn main() -> anyhow::Result<()> {
    let mnemonic = std::env::var("VOTE_SDK_VOTER_TEST").unwrap_or_default();
    let mnemonic = mnemonic.trim();
    anyhow::ensure!(!mnemonic.is_empty(), "VOTE_SDK_VOTER_TEST is unset");

    let mut seed64 = [0u8; 64];
    pbkdf2::pbkdf2::<hmac::Hmac<sha2::Sha512>>(mnemonic.as_bytes(), b"mnemonic", 2048, &mut seed64)
        .map_err(|error| anyhow::anyhow!("deriving seed: {error}"))?;

    let entropy = entropy_from(mnemonic)?;

    for (label, seed) in [
        ("64-byte BIP39 seed", &seed64[..]),
        ("32-byte entropy", &entropy[..]),
    ] {
        for account in 0..3u32 {
            let index = zip32::AccountId::try_from(account)
                .map_err(|_| anyhow::anyhow!("bad account index"))?;
            if let Ok(usk) = UnifiedSpendingKey::from_seed(&Network::Testnet, seed, index) {
                println!(
                    "{label} account {account}: {}",
                    usk.to_unified_full_viewing_key().encode(&Network::Testnet)
                );
            }
        }
    }
    Ok(())
}

/// Recovers the BIP39 entropy the mnemonic encodes.
fn entropy_from(mnemonic: &str) -> anyhow::Result<Vec<u8>> {
    let words: Vec<&str> = WORDLIST
        .lines()
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .collect();
    let mut bits = String::new();
    for word in mnemonic.split_whitespace() {
        let index = words
            .iter()
            .position(|candidate| *candidate == word)
            .ok_or_else(|| anyhow::anyhow!("word not in BIP39 wordlist"))?;
        bits.push_str(&format!("{index:011b}"));
    }
    let entropy_bits = bits.len() - bits.len() / 33;
    Ok(bits.as_bytes()[..entropy_bits]
        .chunks(8)
        .map(|chunk| {
            chunk
                .iter()
                .fold(0u8, |acc, bit| (acc << 1) | u8::from(*bit == b'1'))
        })
        .collect())
}
