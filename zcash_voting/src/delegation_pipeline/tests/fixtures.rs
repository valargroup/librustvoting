use std::sync::Arc;

use crate::{
    backend::{
        pasta_curves::{
            group::{Group, GroupEncoding},
            pallas,
        },
        zcash_client_sqlite,
    },
    delegate::{DelegationLwdInputs, LightwalletdBranchIdProvider},
    delegation_pipeline::{DelegationPipeline, WalletDbOpener},
    note_bundling::BundlePolicy,
    round::VotingDb,
    storage::queries,
    Network, NoteInfo, VotingError, VotingRoundParams,
};

pub(super) const ROUND_ID: &str =
    "0101010101010101010101010101010101010101010101010101010101010101";
pub(super) const WALLET_ID: &str = "pipeline-wallet";
const SNAPSHOT_HEIGHT: u64 = 3_000_000;

/// A wallet opener for stages that must never reach the wallet database.
pub(super) struct NoWalletDatabase;

impl WalletDbOpener for NoWalletDatabase {
    type Conn = rusqlite::Connection;
    type Params = Network;
    type Clock = zcash_client_sqlite::util::SystemClock;
    type Rng = voting_crypto_deps::rand::rngs::OsRng;

    fn open_for_read(
        &self,
    ) -> Result<
        zcash_client_sqlite::WalletDb<Self::Conn, Self::Params, Self::Clock, Self::Rng>,
        VotingError,
    > {
        Err(VotingError::Internal {
            message: "this test pipeline has no wallet database".to_string(),
        })
    }
}

pub(super) fn round_params() -> VotingRoundParams {
    VotingRoundParams {
        vote_round_id: ROUND_ID.to_string(),
        snapshot_height: SNAPSHOT_HEIGHT,
        ea_pk: pallas::Point::generator().to_bytes().to_vec(),
        nc_root: vec![0x31; 32],
        nullifier_imt_root: vec![0x32; 32],
    }
}

fn bundle_note() -> NoteInfo {
    NoteInfo {
        commitment: vec![0x11; 32],
        nullifier: vec![0x12; 32],
        value: 13_000_000,
        position: 7,
        diversifier: vec![0x13; 11],
        rho: vec![0x14; 32],
        rseed: vec![0x15; 32],
        scope: 0,
        ufvk_str: "uview1pipelinefixture".to_string(),
    }
}

/// A pipeline bound to an in-memory sidecar whose round row and bundle 0
/// already exist.
pub(super) fn pipeline_with_round() -> DelegationPipeline<NoWalletDatabase> {
    let voting_db = Arc::new(VotingDb::open_in_memory().unwrap());
    voting_db.set_wallet_id(WALLET_ID);
    queries::insert_round(
        &voting_db.conn(),
        WALLET_ID,
        Network::Testnet,
        &round_params(),
        None,
    )
    .unwrap();
    queries::insert_bundle_notes(&voting_db.conn(), ROUND_ID, WALLET_ID, 0, &[bundle_note()])
        .unwrap();
    let lwd = DelegationLwdInputs {
        network: Network::Testnet,
        round_params: round_params(),
        resolved_round_name: "pipeline test round".to_string(),
        anchor_tree_state_bytes: Vec::new(),
        branch_id_provider: LightwalletdBranchIdProvider::for_height(
            Network::Testnet,
            SNAPSHOT_HEIGHT,
        )
        .unwrap(),
    };
    DelegationPipeline::new(
        voting_db,
        NoWalletDatabase,
        lwd,
        "pipeline-account",
        None,
        BundlePolicy::default(),
        None,
    )
    .unwrap()
}
