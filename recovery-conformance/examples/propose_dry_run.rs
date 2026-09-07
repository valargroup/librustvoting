//! Validates a full round description against live staging without changing
//! anything: resolves the snapshot anchor, imports the coordinator key into a
//! throwaway keyring, and simulates `MsgCreateVotingSession`.
use recovery_conformance::environment::{
    LIGHTWALLETD_URLS, STAGING_CHAIN_ID, STAGING_CHAIN_RPC, ZCASH_NETWORK,
};
use recovery_conformance::provisioning::{
    propose_round, resolve_anchor, suite_ballot, ChainTarget, Dispatch, RoundDescription,
    VoteManagerKeyring, COORDINATOR_ADDRESS,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let anchor = resolve_anchor(
        "https://stage.pir.valargroup.org",
        LIGHTWALLETD_URLS[0],
        ZCASH_NETWORK,
    )
    .await?;
    println!("anchor height {} ok", anchor.height);

    // Two weeks out: long enough that a slow proving run cannot cross the
    // vote-end boundary mid-suite, which would fail casts for a reason that
    // has nothing to do with recovery.
    let vote_end = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64
        + 14 * 24 * 3600;

    let description = RoundDescription::new(&anchor, vote_end, suite_ballot());
    let path = std::env::temp_dir().join("recovery-conformance-round.json");
    std::fs::write(&path, description.to_json()?)?;
    println!("proposals_hash {}", description.proposals_hash);

    let mnemonic = std::env::var("VOTE_MANAGER_VOTE_SDK").unwrap_or_default();
    let keyring = VoteManagerKeyring::import(mnemonic.trim())?;
    println!(
        "signing as {} (expected {COORDINATOR_ADDRESS})",
        keyring.address()
    );

    let target = ChainTarget {
        rpc_url: STAGING_CHAIN_RPC,
        chain_id: STAGING_CHAIN_ID,
    };
    match propose_round(&keyring, &path, &target, Dispatch::DryRun) {
        Ok(_) => println!("DRY RUN OK: the chain accepts this round description"),
        Err(error) => {
            println!("DRY RUN FAILED: {error:#}");
            std::process::exit(1);
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}
