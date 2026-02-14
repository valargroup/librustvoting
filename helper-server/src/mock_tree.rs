//! Mock tree dev server: wraps [`TreeServer`] in HTTP endpoints matching the
//! real chain's REST API. This allows `HttpTreeSyncApi` (and eventually iOS)
//! to sync against a local in-memory tree during development.
//!
//! ## Chain-compatible endpoints
//! - `GET /zally/v1/commitment-tree/latest`
//! - `GET /zally/v1/commitment-tree/{height}`
//! - `GET /zally/v1/commitment-tree/leaves?from_height=X&to_height=Y`
//!
//! ## Admin endpoints (testing)
//! - `POST /admin/append` — insert leaf(s) + checkpoint
//! - `GET  /admin/status` — tree size, checkpoint count

use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use axum::Router;
use base64::prelude::*;
use ff::PrimeField;
use pasta_curves::Fp;
use serde::{Deserialize, Serialize};

use vote_commitment_tree::sync_api::TreeSyncApi;
use vote_commitment_tree::TreeServer;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Shared mock tree state.
#[derive(Clone)]
pub struct MockTreeState {
    tree: Arc<Mutex<TreeServer>>,
    /// Next block height for auto-checkpointing via admin API.
    next_height: Arc<Mutex<u32>>,
}

impl MockTreeState {
    pub fn new() -> Self {
        Self {
            tree: Arc::new(Mutex::new(TreeServer::empty())),
            next_height: Arc::new(Mutex::new(1)),
        }
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> Router {
    let state = MockTreeState::new();

    Router::new()
        // Chain-compatible endpoints (what HttpTreeSyncApi calls).
        .route(
            "/zally/v1/commitment-tree/latest",
            get(handle_latest),
        )
        .route(
            "/zally/v1/commitment-tree/leaves",
            get(handle_leaves),
        )
        .route(
            "/zally/v1/commitment-tree/{height}",
            get(handle_at_height),
        )
        // Admin endpoints.
        .route("/admin/append", post(handle_admin_append))
        .route("/admin/status", get(handle_admin_status))
        // Mock chain submission endpoint (MsgRevealShare).
        .route("/zally/v1/reveal-share", post(handle_reveal_share))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Chain-compatible handlers
// ---------------------------------------------------------------------------

/// JSON shapes matching the Go chain's protobuf JSON serialization.
#[derive(Serialize)]
struct TreeStateJson {
    next_index: u64,
    root: String,
    height: u64,
}

#[derive(Serialize)]
struct LatestResponse {
    tree: TreeStateJson,
}

#[derive(Serialize)]
struct HeightResponse {
    tree: Option<TreeStateJson>,
}

#[derive(Serialize)]
struct BlockJson {
    height: u64,
    start_index: u64,
    leaves: Vec<String>,
}

#[derive(Serialize)]
struct LeavesResponse {
    blocks: Vec<BlockJson>,
}

fn fp_to_b64(fp: Fp) -> String {
    BASE64_STANDARD.encode(fp.to_repr())
}

async fn handle_latest(State(state): State<MockTreeState>) -> Json<LatestResponse> {
    let tree = state.tree.lock().unwrap();
    let ts = tree.get_tree_state().unwrap();
    Json(LatestResponse {
        tree: TreeStateJson {
            next_index: ts.next_index,
            root: fp_to_b64(ts.root),
            height: ts.height as u64,
        },
    })
}

async fn handle_at_height(
    State(state): State<MockTreeState>,
    Path(height): Path<u32>,
) -> Json<HeightResponse> {
    let tree = state.tree.lock().unwrap();
    let root = tree.get_root_at_height(height).unwrap();
    let ts = tree.get_tree_state().unwrap();
    Json(HeightResponse {
        tree: root.map(|r| TreeStateJson {
            next_index: ts.next_index,
            root: fp_to_b64(r),
            height: height as u64,
        }),
    })
}

#[derive(Deserialize)]
struct LeavesQuery {
    from_height: u32,
    to_height: u32,
}

async fn handle_leaves(
    State(state): State<MockTreeState>,
    Query(params): Query<LeavesQuery>,
) -> Json<LeavesResponse> {
    let tree = state.tree.lock().unwrap();
    let blocks = tree
        .get_block_commitments(params.from_height, params.to_height)
        .unwrap();

    let blocks_json: Vec<BlockJson> = blocks
        .into_iter()
        .map(|b| BlockJson {
            height: b.height as u64,
            start_index: b.start_index,
            leaves: b.leaves.iter().map(|l| fp_to_b64(l.inner())).collect(),
        })
        .collect();

    Json(LeavesResponse {
        blocks: blocks_json,
    })
}

// ---------------------------------------------------------------------------
// Admin handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AdminAppendRequest {
    /// Base64-encoded 32-byte Pallas Fp values.
    leaves: Vec<String>,
}

#[derive(Serialize)]
struct AdminAppendResponse {
    start_index: u64,
    leaves_added: usize,
    height: u32,
}

async fn handle_admin_append(
    State(state): State<MockTreeState>,
    Json(req): Json<AdminAppendRequest>,
) -> Result<Json<AdminAppendResponse>, (StatusCode, String)> {
    if req.leaves.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no leaves provided".into()));
    }

    // Decode all leaves first.
    let mut fps = Vec::with_capacity(req.leaves.len());
    for (i, b64) in req.leaves.iter().enumerate() {
        let bytes = BASE64_STANDARD
            .decode(b64)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("leaf {}: base64: {}", i, e)))?;
        if bytes.len() != 32 {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("leaf {}: expected 32 bytes, got {}", i, bytes.len()),
            ));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        let fp = Option::from(Fp::from_repr(arr)).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                format!("leaf {}: non-canonical Fp", i),
            )
        })?;
        fps.push(fp);
    }

    let mut tree = state.tree.lock().unwrap();
    let start = tree.size();
    for fp in &fps {
        tree.append(*fp);
    }

    let mut height = state.next_height.lock().unwrap();
    tree.checkpoint(*height);
    let h = *height;
    *height += 1;

    Ok(Json(AdminAppendResponse {
        start_index: start,
        leaves_added: fps.len(),
        height: h,
    }))
}

#[derive(Serialize)]
struct AdminStatusResponse {
    tree_size: u64,
    #[serde(serialize_with = "serialize_root")]
    root: Fp,
    latest_height: u32,
}

fn serialize_root<S: serde::Serializer>(fp: &Fp, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&fp_to_b64(*fp))
}

async fn handle_admin_status(State(state): State<MockTreeState>) -> Json<AdminStatusResponse> {
    let tree = state.tree.lock().unwrap();
    let ts = tree.get_tree_state().unwrap();
    Json(AdminStatusResponse {
        tree_size: ts.next_index,
        root: ts.root,
        latest_height: ts.height,
    })
}

// ---------------------------------------------------------------------------
// Mock chain submission (MsgRevealShare)
// ---------------------------------------------------------------------------

/// Accept MsgRevealShare and log it. In the real chain this would broadcast
/// via CometBFT; here we just acknowledge.
async fn handle_reveal_share(
    body: axum::body::Bytes,
) -> Json<serde_json::Value> {
    tracing::info!(
        bytes = body.len(),
        "received MsgRevealShare submission"
    );
    Json(serde_json::json!({
        "tx_hash": format!("mock_{:016x}", rand::random::<u64>()),
        "code": 0
    }))
}
