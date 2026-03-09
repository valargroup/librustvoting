use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Bytes;
use tokio::sync::RwLock;

use pir_export::PirMetadata;
use pir_server::{OwnedTierState, YpirScenario};

#[derive(Clone, serde::Serialize)]
#[serde(tag = "phase")]
pub(crate) enum ServerPhase {
    #[serde(rename = "serving")]
    Serving,
    #[serde(rename = "rebuilding")]
    Rebuilding {
        target_height: u64,
        progress: String,
        progress_pct: u8,
    },
    #[serde(rename = "error")]
    Error { message: String },
}

/// All data needed to serve PIR queries. Replaced atomically on rebuild.
///
/// Raw tier data is NOT kept in memory — the YPIR server copies it into its own
/// internal representation during construction, so we drop the source bytes.
/// Hints and tier0 use `Bytes` (reference-counted) to avoid cloning on each
/// HTTP response.
pub(crate) struct ServingState {
    pub tier0_data: Bytes,
    pub tier1: OwnedTierState,
    pub tier2: OwnedTierState,
    pub tier1_scenario: YpirScenario,
    pub tier2_scenario: YpirScenario,
    pub tier1_hint: Bytes,
    pub tier2_hint: Bytes,
    pub metadata: PirMetadata,
}

pub(crate) struct AppState {
    pub phase: RwLock<ServerPhase>,
    pub serving: RwLock<Option<ServingState>>,
    /// Prevents concurrent rebuilds. Held for the entire duration of a rebuild task.
    /// Wrapped in Arc so we can obtain an OwnedMutexGuard that is 'static.
    pub rebuild_lock: Arc<tokio::sync::Mutex<()>>,
    pub data_dir: PathBuf,
    pub pir_data_dir: PathBuf,
    pub lwd_urls: Vec<String>,
    pub chain_url: Option<String>,
    pub next_req_id: AtomicU64,
    pub inflight_requests: AtomicUsize,
}

pub(crate) struct InflightGuard<'a> {
    inflight: &'a AtomicUsize,
}

impl<'a> InflightGuard<'a> {
    pub fn new(inflight: &'a AtomicUsize) -> Self {
        Self { inflight }
    }
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Acquire the serving state read-guard or return 503 if unavailable (during rebuild).
macro_rules! require_serving {
    ($state:expr) => {{
        let guard = $state.serving.read().await;
        if guard.is_none() {
            let phase = $state.phase.read().await;
            let body = serde_json::to_string(&*phase).unwrap_or_default();
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response();
        }
        guard
    }};
}
