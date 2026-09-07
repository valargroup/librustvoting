//! Reads the provisioned round back off the chain, bypassing config auth.
use recovery_conformance::environment::STAGING_CHAIN_RPC;
use recovery_conformance::provisioning::fetch_round;

fn main() -> anyhow::Result<()> {
    let round_id = std::env::args()
        .nth(1)
        .expect("usage: fetch_round <round-id>");
    let round = fetch_round(STAGING_CHAIN_RPC, &round_id)?;
    println!("vote_round_id      : {}", round.params.vote_round_id);
    println!("snapshot_height    : {}", round.params.snapshot_height);
    println!("ea_pk              : {}", hex(&round.params.ea_pk));
    println!("nc_root            : {}", hex(&round.params.nc_root));
    println!(
        "nullifier_imt_root : {}",
        hex(&round.params.nullifier_imt_root)
    );
    println!(
        "status             : {} (active: {})",
        round.status,
        round.is_active()
    );
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
