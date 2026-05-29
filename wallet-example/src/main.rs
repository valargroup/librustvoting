use zcash_voting_wallet_example::example::PRECOMPUTE_FLOW;

fn main() {
    println!("zcash_voting wallet precompute example");
    println!();
    println!("Run this crate with:");
    println!("  cargo run -p zcash-voting-wallet-example");
    println!();
    println!("Current scaffold:");

    for (index, step) in PRECOMPUTE_FLOW.iter().enumerate() {
        println!("  {}. {step}", index + 1);
    }

    println!();
    println!(
        "The full example lives in zcash_voting_wallet_example::example::precompute_delegation_bundle."
    );
    println!("Next increments can wire CLI/env inputs into real wallet DB and PIR state.");
}
