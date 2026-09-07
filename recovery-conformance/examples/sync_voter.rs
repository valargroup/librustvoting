//! Builds the voter wallet by scanning testnet, and reports what it found.
//!
//! Scans only up to the round's snapshot height: a note mined after it is not
//! in the round's nullifier set and cannot be delegated, so scanning further
//! could not change the answer.
use recovery_conformance::environment::{LIGHTWALLETD_URLS, ZCASH_NETWORK};
use recovery_conformance::wallet_sync::sync_wallet;

const WORDLIST: &str = include_str!("bip39-english.txt");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let from: u64 = std::env::args()
        .nth(1)
        .map_or(4_237_303, |a| a.parse().unwrap());
    let to: u64 = std::env::args()
        .nth(2)
        .map_or(4_245_460, |a| a.parse().unwrap());

    let mnemonic = std::env::var("VOTE_SDK_VOTER_TEST").unwrap_or_default();
    let mnemonic = mnemonic.trim();
    anyhow::ensure!(!mnemonic.is_empty(), "VOTE_SDK_VOTER_TEST is unset");
    let _ = WORDLIST;

    let mut seed = [0u8; 64];
    pbkdf2::pbkdf2::<hmac::Hmac<sha2::Sha512>>(mnemonic.as_bytes(), b"mnemonic", 2048, &mut seed)
        .map_err(|e| anyhow::anyhow!("deriving seed: {e}"))?;

    let path = std::path::PathBuf::from("/tmp/recovery-conformance-voter.db");
    let _ = std::fs::remove_file(&path);

    println!("scanning {from}..={to} ({} blocks)", to - from + 1);
    let synced = sync_wallet(&path, &seed, LIGHTWALLETD_URLS[0], ZCASH_NETWORK, from, to).await?;
    println!("wallet   : {}", synced.path.display());
    println!("account  : {}", synced.account_uuid);
    println!("scanned  : {}", synced.scanned_to);

    let notes: i64 = rusqlite::Connection::open(&path)?
        .query_row("select count(*) from ironwood_received_notes", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    let value: i64 = rusqlite::Connection::open(&path)?
        .query_row(
            "select coalesce(sum(value),0) from ironwood_received_notes",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    println!("NOTES FOUND: {notes}  total {value} zat");
    Ok(())
}
