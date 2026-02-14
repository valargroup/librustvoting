//! Background share processing pipeline.
//!
//! Periodically checks the share queue for shares whose delay has elapsed,
//! then for each ready share:
//! 1. Syncs the tree (if needed)
//! 2. Generates VC Merkle witness at latest anchor height
//! 3. Derives share_nullifier
//! 4. Generates ZKP #3 proof (currently mocked)
//! 5. POSTs MsgRevealShare to the chain endpoint

use base64::prelude::*;
use ff::PrimeField;
use pasta_curves::Fp;

use crate::chain_client::ChainClient;
use crate::nullifier::derive_share_nullifier;
use crate::store::ShareStore;
use crate::tree::TreeSync;
use crate::types::{MsgRevealShareJson, QueuedShare};

/// Run the share processing loop.
pub async fn run_processor(
    store: ShareStore,
    tree: TreeSync,
    chain: ChainClient,
    interval_secs: u64,
) {
    let interval = std::time::Duration::from_secs(interval_secs);
    loop {
        let ready = store.take_ready();
        if !ready.is_empty() {
            tracing::info!(count = ready.len(), "processing ready shares");

            // Sync tree before processing batch.
            let tree_clone = tree.clone();
            if let Err(e) = tokio::task::spawn_blocking(move || tree_clone.sync()).await {
                tracing::error!(error = %e, "tree sync for processing failed");
            }

            for share in ready {
                match process_share(&share, &tree, &chain).await {
                    Ok(()) => {
                        store.mark_submitted(
                            &share.payload.vote_round_id,
                            share.payload.enc_share.share_index,
                        );
                        tracing::info!(
                            round_id = %share.payload.vote_round_id,
                            share_index = share.payload.enc_share.share_index,
                            "share submitted"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            round_id = %share.payload.vote_round_id,
                            share_index = share.payload.enc_share.share_index,
                            error = %e,
                            "share processing failed"
                        );
                        store.mark_failed(
                            &share.payload.vote_round_id,
                            share.payload.enc_share.share_index,
                        );
                    }
                }
            }
        }
        tokio::time::sleep(interval).await;
    }
}

/// Process a single share: witness → nullifier → proof → submit.
async fn process_share(
    share: &QueuedShare,
    tree: &TreeSync,
    chain: &ChainClient,
) -> Result<(), String> {
    let anchor_height = tree
        .latest_height()
        .ok_or("tree not synced yet (no checkpoint)")?;

    // Generate VC Merkle witness.
    let _witness = tree
        .witness(share.payload.tree_position, anchor_height)
        .ok_or_else(|| {
            format!(
                "no witness for position {} at height {}",
                share.payload.tree_position, anchor_height
            )
        })?;

    // Reconstruct vote_commitment (VC) leaf value from the tree.
    // For nullifier derivation we need the actual leaf Fp value.
    // Since TreeClient doesn't expose leaf values directly, we derive the VC
    // from the shares_hash. In production this will come from the circuit's
    // public inputs. For now, use shares_hash as a proxy for the VC.
    let vc_bytes = BASE64_STANDARD
        .decode(&share.payload.shares_hash)
        .map_err(|e| format!("decode shares_hash: {}", e))?;
    let mut vc_arr = [0u8; 32];
    vc_arr.copy_from_slice(&vc_bytes);
    let vote_commitment =
        Option::from(Fp::from_repr(vc_arr)).ok_or("non-canonical shares_hash Fp")?;

    // Derive share nullifier.
    let nullifier = derive_share_nullifier(&share.payload, vote_commitment)
        .ok_or("nullifier derivation failed")?;

    // Build MsgRevealShare.
    // enc_share on chain is C1 || C2 (64 bytes).
    let c1 = BASE64_STANDARD
        .decode(&share.payload.enc_share.c1)
        .map_err(|e| format!("decode c1: {}", e))?;
    let c2 = BASE64_STANDARD
        .decode(&share.payload.enc_share.c2)
        .map_err(|e| format!("decode c2: {}", e))?;
    let mut enc_share_bytes = Vec::with_capacity(64);
    enc_share_bytes.extend_from_slice(&c1);
    enc_share_bytes.extend_from_slice(&c2);

    // Convert vote_round_id from hex to base64 (chain expects base64).
    let round_id_bytes = hex::decode(&share.payload.vote_round_id)
        .map_err(|e| format!("decode vote_round_id: {}", e))?;

    let msg = MsgRevealShareJson {
        share_nullifier: BASE64_STANDARD.encode(nullifier.to_repr()),
        enc_share: BASE64_STANDARD.encode(&enc_share_bytes),
        proposal_id: share.payload.proposal_id,
        vote_decision: share.payload.vote_decision,
        // ZKP #3 proof — mocked until the Halo2 circuit exists.
        proof: BASE64_STANDARD.encode(vec![0u8; 192]),
        vote_round_id: BASE64_STANDARD.encode(&round_id_bytes),
        vote_comm_tree_anchor_height: anchor_height as u64,
    };

    let result = chain.submit_reveal_share(&msg).await?;
    if result.code != 0 {
        return Err(format!(
            "chain rejected tx (code {}): {}",
            result.code, result.log
        ));
    }

    tracing::debug!(tx_hash = %result.tx_hash, "MsgRevealShare broadcast ok");
    Ok(())
}
