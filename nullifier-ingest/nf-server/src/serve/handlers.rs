use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::IntoResponse;
use std::io::{Read, Seek, SeekFrom};
use tracing::{info, warn};

use pir_server::{
    HealthInfo, QueryTiming, RootInfo,
    TIER1_ROWS, TIER1_ROW_BYTES, TIER2_ROWS, TIER2_ROW_BYTES,
};

use super::state::{AppState, InflightGuard, ServerPhase};

// ── PIR data endpoints ───────────────────────────────────────────────────────

pub(crate) async fn get_tier0(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let guard = require_serving!(state);
    let s = guard.as_ref().unwrap();
    (
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        s.tier0_data.clone(),
    )
        .into_response()
}

pub(crate) async fn get_params_tier1(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let guard = require_serving!(state);
    let s = guard.as_ref().unwrap();
    axum::Json(s.tier1_scenario.clone()).into_response()
}

pub(crate) async fn get_params_tier2(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let guard = require_serving!(state);
    let s = guard.as_ref().unwrap();
    axum::Json(s.tier2_scenario.clone()).into_response()
}

pub(crate) async fn get_hint_tier1(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let guard = require_serving!(state);
    let s = guard.as_ref().unwrap();
    (
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        s.tier1_hint.clone(),
    )
        .into_response()
}

pub(crate) async fn get_hint_tier2(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let guard = require_serving!(state);
    let s = guard.as_ref().unwrap();
    (
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        s.tier2_hint.clone(),
    )
        .into_response()
}

// ── YPIR query endpoints ─────────────────────────────────────────────────────

pub(crate) async fn post_tier1_query(State(state): State<Arc<AppState>>, body: Bytes) -> impl IntoResponse {
    post_tier_query(&state, "tier1", body).await
}

pub(crate) async fn post_tier2_query(State(state): State<Arc<AppState>>, body: Bytes) -> impl IntoResponse {
    post_tier_query(&state, "tier2", body).await
}

async fn post_tier_query(state: &AppState, tier: &str, body: Bytes) -> axum::response::Response {
    let req_id = state.next_req_id.fetch_add(1, Ordering::Relaxed) + 1;
    let inflight = state.inflight_requests.fetch_add(1, Ordering::Relaxed) + 1;
    let _inflight_guard = InflightGuard::new(&state.inflight_requests);
    let t0 = Instant::now();

    let guard = state.serving.read().await;
    if guard.is_none() {
        let phase = state.phase.read().await;
        let body = serde_json::to_string(&*phase).unwrap_or_default();
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response();
    }
    let s = guard.as_ref().unwrap();

    info!(req_id, tier, body_bytes = body.len(), inflight_requests = inflight, "pir_request_started");

    let server = match tier {
        "tier1" => s.tier1.server(),
        "tier2" => s.tier2.server(),
        _ => unreachable!(),
    };

    match server.answer_query(&body) {
        Ok(answer) => {
            let handler_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let mut response = (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
                answer.response,
            )
                .into_response();
            write_timing_headers(response.headers_mut(), req_id, answer.timing);
            info!(
                req_id,
                tier,
                status = 200,
                handler_ms = format!("{handler_ms:.3}"),
                validate_ms = format!("{:.3}", answer.timing.validate_ms),
                decode_copy_ms = format!("{:.3}", answer.timing.decode_copy_ms),
                compute_ms = format!("{:.3}", answer.timing.online_compute_ms),
                server_total_ms = format!("{:.3}", answer.timing.total_ms),
                response_bytes = answer.timing.response_bytes,
                "pir_request_finished"
            );
            response
        }
        Err(e) => {
            warn!(
                req_id,
                tier,
                status = 400,
                handler_ms = format!("{:.3}", t0.elapsed().as_secs_f64() * 1000.0),
                error = %e,
                "pir_request_failed"
            );
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
    }
}

fn write_timing_headers(headers: &mut axum::http::HeaderMap, req_id: u64, timing: QueryTiming) {
    headers.insert("x-pir-req-id", HeaderValue::from_str(&req_id.to_string()).expect("req_id header"));
    headers.insert("x-pir-server-total-ms", HeaderValue::from_str(&format!("{:.3}", timing.total_ms)).expect("timing header"));
    headers.insert("x-pir-server-validate-ms", HeaderValue::from_str(&format!("{:.3}", timing.validate_ms)).expect("timing header"));
    headers.insert("x-pir-server-decode-copy-ms", HeaderValue::from_str(&format!("{:.3}", timing.decode_copy_ms)).expect("timing header"));
    headers.insert("x-pir-server-compute-ms", HeaderValue::from_str(&format!("{:.3}", timing.online_compute_ms)).expect("timing header"));
    headers.insert("x-pir-server-response-bytes", HeaderValue::from_str(&timing.response_bytes.to_string()).expect("timing header"));
}

// ── Tier row endpoints (raw row reads for debugging) ─────────────────────────

pub(crate) async fn get_tier1_row(
    State(state): State<Arc<AppState>>,
    Path(idx): Path<usize>,
) -> impl IntoResponse {
    let _guard = require_serving!(state);
    if idx >= TIER1_ROWS {
        return (StatusCode::NOT_FOUND, "row index out of range").into_response();
    }
    let path = state.pir_data_dir.join("tier1.bin");
    let offset = (idx * TIER1_ROW_BYTES) as u64;
    match read_tier_row(&path, offset, TIER1_ROW_BYTES) {
        Ok(row) => (
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            row,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("read error: {e}")).into_response(),
    }
}

pub(crate) async fn get_tier2_row(
    State(state): State<Arc<AppState>>,
    Path(idx): Path<usize>,
) -> impl IntoResponse {
    let _guard = require_serving!(state);
    if idx >= TIER2_ROWS {
        return (StatusCode::NOT_FOUND, "row index out of range").into_response();
    }
    let path = state.pir_data_dir.join("tier2.bin");
    let offset = (idx * TIER2_ROW_BYTES) as u64;
    match read_tier_row(&path, offset, TIER2_ROW_BYTES) {
        Ok(row) => (
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            row,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("read error: {e}")).into_response(),
    }
}

fn read_tier_row(path: &std::path::Path, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

// ── Root and health ──────────────────────────────────────────────────────────

pub(crate) async fn get_root(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let guard = require_serving!(state);
    let s = guard.as_ref().unwrap();
    let info = RootInfo {
        root29: s.metadata.root29.clone(),
        root26: s.metadata.root26.clone(),
        num_ranges: s.metadata.num_ranges,
        pir_depth: s.metadata.pir_depth,
        height: s.metadata.height,
    };
    axum::Json(info).into_response()
}

pub(crate) async fn get_health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let phase = state.phase.read().await;
    let serving = state.serving.read().await;

    let status = match &*phase {
        ServerPhase::Serving => "ok",
        ServerPhase::Rebuilding { .. } => "rebuilding",
        ServerPhase::Error { .. } => "error",
    };

    let (tier1_rows, tier2_rows) = match serving.as_ref() {
        Some(s) => (s.tier1_scenario.num_items, s.tier2_scenario.num_items),
        None => (0, 0),
    };

    let info = HealthInfo {
        status: status.to_string(),
        tier1_rows,
        tier2_rows,
        tier1_row_bytes: TIER1_ROW_BYTES,
        tier2_row_bytes: TIER2_ROW_BYTES,
    };
    axum::Json(info)
}
