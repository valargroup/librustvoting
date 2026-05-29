use std::env;

use anyhow::{Context, Result};
use rand::rngs::OsRng;
use zcash_client_sqlite::{util::SystemClock, WalletDb};
use zcash_voting::prelude::VotingDb;
use zcash_voting::{Network, VotingRoundParams};
use zcash_voting_wallet_example::example::{
    precompute_delegation_bundle, WalletPrecomputeRequest, PRECOMPUTE_FLOW,
};

#[tokio::main]
async fn main() -> Result<()> {
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

    if env::var_os("ZVOTING_RUN_PRECOMPUTE").is_none() {
        println!("Set ZVOTING_RUN_PRECOMPUTE=1 and the required env vars to call it.");
        return Ok(());
    }

    let config = EnvConfig::from_env()?;
    let wallet_db = WalletDb::for_path(&config.wallet_db_path, config.network, SystemClock, OsRng)
        .context("open wallet DB")?;
    let voting_db = VotingDb::open(&config.voting_db_path).context("open voting DB")?;
    voting_db.set_wallet_id(&config.wallet_id);

    let report = precompute_delegation_bundle(
        &voting_db,
        &wallet_db,
        WalletPrecomputeRequest {
            account_uuid: &config.account_uuid,
            lightwalletd_url: &config.lightwalletd_url,
            round_params: config.round_params,
            round_name: &config.round_name,
            hotkey_raw_address: config.hotkey_raw_address,
            scanned_height: config.scanned_height,
            network: config.network,
            pir_server_url: &config.pir_server_url,
            bundle_index: config.bundle_index,
        },
    )
    .await?;

    println!(
        "Precomputed bundle {}: {} cached PIR rows, {} fetched PIR rows",
        report.bundle_index, report.report.cached, report.report.fetched
    );

    Ok(())
}

struct EnvConfig {
    wallet_db_path: String,
    voting_db_path: String,
    wallet_id: String,
    account_uuid: String,
    lightwalletd_url: String,
    pir_server_url: String,
    network: Network,
    round_name: String,
    round_params: VotingRoundParams,
    hotkey_raw_address: Vec<u8>,
    scanned_height: u64,
    bundle_index: u32,
}

impl EnvConfig {
    fn from_env() -> Result<Self> {
        Ok(Self {
            wallet_db_path: required_env("ZVOTING_WALLET_DB")?,
            voting_db_path: required_env("ZVOTING_VOTING_DB")?,
            wallet_id: required_env("ZVOTING_WALLET_ID")?,
            account_uuid: required_env("ZVOTING_ACCOUNT_UUID")?,
            lightwalletd_url: required_env("ZVOTING_LIGHTWALLETD_URL")?,
            pir_server_url: required_env("ZVOTING_PIR_SERVER_URL")?,
            network: parse_network(&required_env("ZVOTING_NETWORK")?)?,
            round_name: required_env("ZVOTING_ROUND_NAME")?,
            round_params: VotingRoundParams {
                vote_round_id: required_env("ZVOTING_ROUND_ID")?,
                snapshot_height: parse_u64_env("ZVOTING_SNAPSHOT_HEIGHT")?,
                ea_pk: parse_hex_env("ZVOTING_EA_PK_HEX")?,
                nc_root: parse_hex_env("ZVOTING_NC_ROOT_HEX")?,
                nullifier_imt_root: parse_hex_env("ZVOTING_NULLIFIER_IMT_ROOT_HEX")?,
            },
            hotkey_raw_address: parse_hex_env("ZVOTING_HOTKEY_RAW_ADDRESS_HEX")?,
            scanned_height: parse_u64_env("ZVOTING_SCANNED_HEIGHT")?,
            bundle_index: parse_u32_env("ZVOTING_BUNDLE_INDEX")?,
        })
    }
}

fn required_env(name: &str) -> Result<String> {
    env::var(name).with_context(|| format!("missing required env var {name}"))
}

fn parse_hex_env(name: &str) -> Result<Vec<u8>> {
    let value = required_env(name)?;
    hex::decode(value.trim_start_matches("0x")).with_context(|| format!("decode {name} as hex"))
}

fn parse_u64_env(name: &str) -> Result<u64> {
    required_env(name)?
        .parse()
        .with_context(|| format!("parse {name} as u64"))
}

fn parse_u32_env(name: &str) -> Result<u32> {
    required_env(name)?
        .parse()
        .with_context(|| format!("parse {name} as u32"))
}

fn parse_network(value: &str) -> Result<Network> {
    match value {
        "mainnet" | "main" => Ok(Network::Mainnet),
        "testnet" | "test" => Ok(Network::Testnet),
        "regtest" => Ok(Network::Regtest),
        _ => anyhow::bail!("ZVOTING_NETWORK must be mainnet, testnet, or regtest"),
    }
}
