//! TreeClient wrapper for background tree sync.
//!
//! Maintains a [`TreeClient`] synced from the chain (or mock tree) via
//! [`HttpTreeSyncApi`]. The tree is synced periodically in a background task,
//! and positions are marked as shares arrive so Merkle witnesses can be
//! generated later.

use std::sync::{Arc, Mutex};

use pasta_curves::Fp;
use vote_commitment_tree::{MerklePath, TreeClient};
use vote_commitment_tree_client::http_sync_api::HttpTreeSyncApi;

// ---------------------------------------------------------------------------
// TreeSync
// ---------------------------------------------------------------------------

/// Thread-safe wrapper around TreeClient + HttpTreeSyncApi.
#[derive(Clone)]
pub struct TreeSync {
    client: Arc<Mutex<TreeClient>>,
    node_url: String,
}

impl TreeSync {
    pub fn new(node_url: String) -> Self {
        Self {
            client: Arc::new(Mutex::new(TreeClient::empty())),
            node_url,
        }
    }

    /// Mark a leaf position for witness retention. Must be called before the
    /// tree syncs past this position.
    pub fn mark_position(&self, position: u64) {
        let mut client = self.client.lock().unwrap();
        client.mark_position(position);
    }

    /// Sync the tree from the remote node. This is a blocking call (uses
    /// reqwest::blocking) and should be run from `spawn_blocking`.
    pub fn sync(&self) -> Result<(), String> {
        let api = HttpTreeSyncApi::new(&self.node_url);
        let mut client = self.client.lock().unwrap();
        client
            .sync(&api)
            .map_err(|e| format!("tree sync failed: {:?}", e))
    }

    /// Generate a Merkle witness for the leaf at `position`, valid at the
    /// given anchor height.
    pub fn witness(&self, position: u64, anchor_height: u32) -> Option<MerklePath> {
        let client = self.client.lock().unwrap();
        client.witness(position, anchor_height)
    }

    /// Latest synced checkpoint height.
    pub fn latest_height(&self) -> Option<u32> {
        let client = self.client.lock().unwrap();
        client.last_synced_height()
    }

    /// Current tree root.
    pub fn root(&self) -> Fp {
        let client = self.client.lock().unwrap();
        client.root()
    }

    /// Number of leaves synced.
    pub fn size(&self) -> u64 {
        let client = self.client.lock().unwrap();
        client.size()
    }

    /// Run the sync loop: periodically sync the tree from the remote node.
    pub async fn run_sync_loop(self, interval_secs: u64) {
        let interval = std::time::Duration::from_secs(interval_secs);
        loop {
            let tree = self.clone();
            match tokio::task::spawn_blocking(move || tree.sync()).await {
                Ok(Ok(())) => {
                    tracing::debug!(
                        height = ?self.latest_height(),
                        size = self.size(),
                        "tree synced"
                    );
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "tree sync error");
                }
                Err(e) => {
                    tracing::error!(error = %e, "tree sync task panicked");
                }
            }
            tokio::time::sleep(interval).await;
        }
    }
}
