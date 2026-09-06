//! Confirms the coordinator mnemonic derives to the account registered on
//! staging. Uses `--dry-run` throughout, so nothing is ever stored.
//!
//! Run with: `infisical run --env=staging -- cargo run --example derive_check -p recovery-conformance`
use recovery_conformance::provisioning::{VoteManagerKeyring, COORDINATOR_ADDRESS};

fn main() {
    let mnemonic = std::env::var("VOTE_MANAGER_VOTE_SDK").unwrap_or_default();
    match VoteManagerKeyring::derive_address(mnemonic.trim()) {
        Ok(address) if address == COORDINATOR_ADDRESS => {
            println!("ok: derived the registered coordinator {address}");
        }
        Ok(address) => {
            println!("MISMATCH");
            println!("  derived : {address}");
            println!("  expected: {COORDINATOR_ADDRESS}");
            println!("  the default derivation is coin type 133, not the cosmos 118");
            std::process::exit(1);
        }
        Err(error) => {
            println!("error: {error:#}");
            std::process::exit(1);
        }
    }
}
