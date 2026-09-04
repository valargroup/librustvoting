use std::sync::Arc;

use crate::{
    backend::pasta_curves::{
        group::{ff::PrimeField, Group, GroupEncoding},
        pallas,
    },
    delegate::DelegationKeys,
    round::VotingDb,
    storage::queries,
    Network, NoteInfo, RoundBoundVotingHotkeyTarget, VotingHotkey, VotingRoundParams,
};

pub(super) const WALLET_A: &str = "proof-wallet-a";
pub(super) const WALLET_B: &str = "proof-wallet-b";
pub(super) const ROUND_ID: &str =
    "0101010101010101010101010101010101010101010101010101010101010101";
pub(super) const WALLET_A_PROOF_BYTE: u8 = 0xA1;
pub(super) const WALLET_B_PROOF_BYTE: u8 = 0xB2;

pub(super) fn note() -> NoteInfo {
    NoteInfo {
        commitment: vec![0x11; 32],
        nullifier: vec![0x12; 32],
        value: 13_000_000,
        position: 7,
        diversifier: vec![0x13; 11],
        rho: vec![0x14; 32],
        rseed: vec![0x15; 32],
        scope: 0,
        ufvk_str: "uview1proofcoordination".to_string(),
    }
}

pub(super) fn keys(network: Network, round_byte: u8) -> DelegationKeys {
    keys_for_hotkey(network, round_byte, 0x21)
}

pub(super) fn keys_for_hotkey(network: Network, round_byte: u8, hotkey_byte: u8) -> DelegationKeys {
    let voting_hotkey = VotingHotkey::from_stored_secret(&[hotkey_byte; 64], network).unwrap();
    let target = RoundBoundVotingHotkeyTarget::from_validated_parts(
        voting_hotkey.delegation_target(),
        "vote-chain-1".to_string(),
        [round_byte; 32],
    );
    DelegationKeys::with_round_bound_voting_target(
        vec![0; 96],
        &target,
        [0x22; 32],
        0,
        "proof coordination round".to_string(),
    )
    .unwrap()
}

pub(super) fn db_with_persisted_proofs() -> VotingDb {
    let db = VotingDb::open_in_memory().unwrap();
    seed_wallet(&db, WALLET_A, WALLET_A_PROOF_BYTE);
    seed_wallet(&db, WALLET_B, WALLET_B_PROOF_BYTE);
    db.set_wallet_id(WALLET_A);
    db
}

pub(super) fn pir_client() -> pir_client::PirClientBlocking {
    pir_client::PirClientBlocking::with_transport(
        "https://pir.test",
        pir_types::COMPILED_PIR_LAYOUT,
        Arc::new(StaticPirTransport),
    )
    .unwrap()
}

fn seed_wallet(db: &VotingDb, wallet_id: &str, proof_byte: u8) {
    let params = VotingRoundParams {
        vote_round_id: ROUND_ID.to_string(),
        snapshot_height: 4_134_000,
        ea_pk: pallas::Point::generator().to_bytes().to_vec(),
        nc_root: vec![0x31; 32],
        nullifier_imt_root: vec![0x32; 32],
    };
    let selected_note = note();
    let delegation_keys = keys(Network::Testnet, 1);
    let (g_d_x, pk_d_x) =
        crate::action::derive_hotkey_x_coords_from_raw_address(&delegation_keys.hotkey_raw_address)
            .unwrap();
    let van_comm_rand = pallas::Base::from(9).to_repr().to_vec();
    let gov_comm = crate::governance::construct_van(
        &g_d_x,
        &pk_d_x,
        13_000_000,
        &hex::decode(ROUND_ID).unwrap(),
        &van_comm_rand,
    )
    .unwrap();
    let rho_signed = pallas::Base::from(7).to_repr();
    let nf_signed = pallas::Base::from(u64::from(proof_byte) + 1).to_repr();
    let rseed_output = [0x44; 32];
    let cmx_new = crate::action::derive_governance_output_cmx(
        &delegation_keys.hotkey_raw_address,
        &nf_signed,
        &rseed_output,
        Network::Testnet,
        params.snapshot_height,
    )
    .unwrap();
    let conn = db.conn();
    queries::insert_round(&conn, wallet_id, Network::Testnet, &params, None).unwrap();
    queries::insert_bundle_notes(&conn, ROUND_ID, wallet_id, 0, &[selected_note]).unwrap();
    queries::store_delegation_data(
        &conn,
        ROUND_ID,
        wallet_id,
        0,
        &van_comm_rand,
        &[],
        &rho_signed,
        &[],
        &nf_signed,
        &cmx_new,
        &[0x42; 32],
        &[0x43; 32],
        &rseed_output,
        &gov_comm,
        13_000_000,
        0,
        &[],
        &[0x45; 32],
        &crate::tx1::placeholder_tx1_effects(),
    )
    .unwrap();
    queries::store_proof(&conn, ROUND_ID, wallet_id, 0, &[proof_byte; 96]).unwrap();
    let gov_nullifiers = vec![vec![proof_byte.wrapping_add(5); 32]; crate::BUNDLE_NOTE_SLOTS];
    queries::store_proof_result_fields_with_van_comm(
        &conn,
        ROUND_ID,
        wallet_id,
        0,
        &[proof_byte.wrapping_add(4); 32],
        &gov_nullifiers,
        &nf_signed,
        &cmx_new,
        &gov_comm,
    )
    .unwrap();
}

struct StaticPirTransport;

impl pir_client::Transport for StaticPirTransport {
    fn get<'a>(&'a self, url: &'a str) -> pir_client::TransportFuture<'a> {
        Box::pin(async move {
            match request_path(url) {
                "/tier0" => Ok(response(vec![
                    0;
                    ((1usize << pir_types::TIER0_LAYERS) - 1) * 32
                        + pir_types::TIER1_ROWS * 64
                ])),
                "/params/tier1" => Ok(response(
                    serde_json::to_vec(&pir_types::YpirScenario {
                        num_items: pir_types::TIER1_ROWS,
                        item_size_bits: pir_types::TIER1_ITEM_BITS,
                        poly_len: pir_types::DEFAULT_YPIR_POLY_LEN,
                    })
                    .unwrap(),
                )),
                "/root" => Ok(response(
                    serde_json::to_vec(&pir_types::RootInfo {
                        zcash_network: pir_types::ZcashNetwork::Test,
                        nullifier_pool: pir_types::NULLIFIER_POOL.to_owned(),
                        dataset_version: pir_types::DATASET_VERSION,
                        circuit_root: hex::encode([0u8; 32]),
                        pir_root: hex::encode([0u8; 32]),
                        num_ranges: 1,
                        pir_layout: pir_types::COMPILED_PIR_LAYOUT,
                        pir_depth: pir_types::PIR_DEPTH,
                        tier1_rows: pir_types::TIER1_ROWS,
                        tier1_row_bytes: pir_types::TIER1_ROW_BYTES,
                        height: None,
                    })
                    .unwrap(),
                )),
                path => Err(anyhow::anyhow!("unexpected GET {path}")),
            }
        })
    }

    fn post<'a>(&'a self, url: &'a str, _body: Vec<u8>) -> pir_client::TransportFuture<'a> {
        Box::pin(async move { Err(anyhow::anyhow!("unexpected POST {}", request_path(url))) })
    }
}

fn request_path(url: &str) -> &str {
    let without_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    without_scheme
        .find('/')
        .map(|index| &without_scheme[index..])
        .unwrap_or("/")
}

fn response(body: Vec<u8>) -> pir_client::TransportResponse {
    pir_client::TransportResponse {
        status: 200,
        headers: Vec::new(),
        body,
    }
}
