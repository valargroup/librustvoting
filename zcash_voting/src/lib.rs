pub mod action;
pub mod decompose;
pub mod elgamal;
pub mod governance;
pub mod hotkey;
#[cfg(feature = "client-pir")]
mod http_transport;
pub mod share_tracking;
pub mod storage;
#[cfg(feature = "client-tree-sync")]
pub mod tree_sync;
pub mod types;
pub mod vote_commitment;
pub mod witness;
pub mod zkp1;
pub mod zkp2;

#[cfg(feature = "client-pir")]
pub use http_transport::HyperTransport;
#[cfg(feature = "client-pir")]
pub use pir_client::{
    ImtProofData, PirClient, PirClientBlocking, Transport, TransportFuture, TransportResponse,
};
pub use types::*;

/// Warm process-lifetime proving-key caches used by on-device voting proofs.
///
/// This is intentionally best-effort at the cache layer: callers should invoke
/// it from a background task before the first proof is needed.
pub fn warm_proving_caches() {
    const KEYGEN_STACK_BYTES: usize = 64 * 1024 * 1024;

    let handles = [
        std::thread::Builder::new()
            .name("voting-delegation-cache-warmup".to_string())
            .stack_size(KEYGEN_STACK_BYTES)
            .spawn(zkp1::warm_delegation_proving_key)
            .expect("spawn delegation proving cache warm-up thread"),
        std::thread::Builder::new()
            .name("voting-vote-proof-cache-warmup".to_string())
            .stack_size(KEYGEN_STACK_BYTES)
            .spawn(voting_circuits::vote_proof::warm_vote_proof_keys)
            .expect("spawn vote proof cache warm-up thread"),
    ];

    for handle in handles {
        handle
            .join()
            .expect("proving cache warm-up thread panicked");
    }
}
