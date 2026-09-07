//! Creates a real round on staging: resolves the anchor, proposes
//! `MsgCreateVotingSession`, and reports the transaction for follow-up.
use recovery_conformance::environment::{
    LIGHTWALLETD_URLS, STAGING_CHAIN_ID, STAGING_CHAIN_RPC, ZCASH_NETWORK,
};
use recovery_conformance::provisioning::{
    propose_round, resolve_anchor, suite_ballot, ChainTarget, Dispatch, ProposalOutcome,
    RoundDescription, VoteManagerKeyring,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let anchor = resolve_anchor(
        "https://stage.pir.valargroup.org",
        LIGHTWALLETD_URLS[0],
        ZCASH_NETWORK,
    )
    .await?;

    let vote_end = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64
        + 14 * 24 * 3600;

    let description = RoundDescription::new(&anchor, vote_end, suite_ballot());
    let path = std::env::temp_dir().join("recovery-conformance-round.json");
    std::fs::write(&path, description.to_json()?)?;

    println!("anchor height      : {}", anchor.height);
    println!("proposals_hash     : {}", description.proposals_hash);
    println!("vote_end_time      : {vote_end}");

    let mnemonic = std::env::var("VOTE_MANAGER_VOTE_SDK").unwrap_or_default();
    let keyring = VoteManagerKeyring::import(mnemonic.trim())?;
    println!("coordinator        : {}", keyring.address());

    let target = ChainTarget {
        rpc_url: STAGING_CHAIN_RPC,
        chain_id: STAGING_CHAIN_ID,
    };
    match propose_round(&keyring, &path, &target, Dispatch::Broadcast)? {
        ProposalOutcome::Pending { transaction_hash }
        | ProposalOutcome::Applied { transaction_hash } => {
            println!("broadcast txhash   : {transaction_hash}");
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}
