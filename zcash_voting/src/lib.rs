pub mod action;
pub mod decompose;
pub mod elgamal;
pub mod governance;
pub mod hotkey;
pub mod share_tracking;
pub mod storage;
pub mod tree_sync;
pub mod types;
pub mod vote_commitment;
pub mod witness;
pub mod zkp1;
pub mod zkp2;

pub use types::*;

/// Warm process-lifetime proving-key caches used by on-device voting proofs.
///
/// This is intentionally best-effort at the cache layer: callers should invoke
/// it from a background task before the first proof is needed.
pub fn warm_proving_caches() {
    const KEYGEN_STACK_BYTES: usize = 64 * 1024 * 1024;

    std::thread::Builder::new()
        .name("voting-delegation-cache-warmup".to_string())
        .stack_size(KEYGEN_STACK_BYTES)
        .spawn(zkp1::warm_delegation_proving_key)
        .expect("spawn delegation proving cache warm-up thread")
        .join()
        .expect("proving cache warm-up thread panicked");
}
