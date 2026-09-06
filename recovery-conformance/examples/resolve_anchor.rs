//! Resolves the snapshot anchor from the staging PIR fleet and lightwalletd.
use recovery_conformance::environment::{LIGHTWALLETD_URLS, ZCASH_NETWORK};
use recovery_conformance::provisioning::resolve_anchor;

#[tokio::main]
async fn main() {
    let pir = "https://stage.pir.valargroup.org";
    match resolve_anchor(pir, LIGHTWALLETD_URLS[0], ZCASH_NETWORK).await {
        Ok(anchor) => {
            println!("snapshot_height    : {}", anchor.height);
            println!("snapshot_blockhash : {}", anchor.blockhash_hex);
            println!("nullifier_imt_root : {}", anchor.nullifier_imt_root_hex);
            println!("nc_root            : {}", anchor.nc_root_hex);
        }
        Err(error) => {
            println!("error: {error:#}");
            std::process::exit(1);
        }
    }
}
