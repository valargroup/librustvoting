use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use tracing::{info, warn};

use pir_server::{OwnedTierState, TIER1_ROWS, TIER1_ROW_BYTES, TIER2_ROWS, TIER2_ROW_BYTES};

use nullifier_service::file_store;
use nullifier_service::sync_nullifiers;

use super::state::{AppState, ServerPhase, ServingState};

/// Load tier files from disk and construct ServingState.
///
/// Raw tier data is read into temporary buffers, passed to `OwnedTierState::new()`
/// (which copies it into YPIR's internal representation), then dropped — saving
/// ~6 GB that was previously kept alive redundantly.
pub(crate) fn load_serving_state(pir_data_dir: &std::path::Path) -> Result<ServingState> {
    let t_total = Instant::now();

    let tier0_data = Bytes::from(std::fs::read(pir_data_dir.join("tier0.bin"))?);
    eprintln!("  Tier 0: {} bytes", tier0_data.len());

    let tier1_data = std::fs::read(pir_data_dir.join("tier1.bin"))?;
    eprintln!(
        "  Tier 1: {} bytes ({} rows)",
        tier1_data.len(),
        tier1_data.len() / TIER1_ROW_BYTES
    );
    anyhow::ensure!(
        tier1_data.len() == TIER1_ROWS * TIER1_ROW_BYTES,
        "tier1.bin size mismatch: got {} bytes, expected {}",
        tier1_data.len(),
        TIER1_ROWS * TIER1_ROW_BYTES
    );

    let tier2_data = std::fs::read(pir_data_dir.join("tier2.bin"))?;
    eprintln!(
        "  Tier 2: {} bytes ({} rows)",
        tier2_data.len(),
        tier2_data.len() / TIER2_ROW_BYTES
    );
    anyhow::ensure!(
        tier2_data.len() == TIER2_ROWS * TIER2_ROW_BYTES,
        "tier2.bin size mismatch: got {} bytes, expected {}",
        tier2_data.len(),
        TIER2_ROWS * TIER2_ROW_BYTES
    );

    let metadata: pir_export::PirMetadata =
        serde_json::from_str(&std::fs::read_to_string(pir_data_dir.join("pir_root.json"))?)?;
    eprintln!(
        "  Metadata: {} ranges, root29={}...",
        metadata.num_ranges,
        metadata.root29.get(..16).unwrap_or(&metadata.root29)
    );

    eprintln!("Initializing YPIR servers...");
    let tier1_scenario = pir_server::tier1_scenario();
    let mut tier1 = OwnedTierState::new(&tier1_data, tier1_scenario.clone());
    drop(tier1_data);
    let tier1_hint = Bytes::from(tier1.take_hint_bytes());
    eprintln!("  Tier 1 YPIR ready (hint: {} bytes)", tier1_hint.len());

    let tier2_scenario = pir_server::tier2_scenario();
    let mut tier2 = OwnedTierState::new(&tier2_data, tier2_scenario.clone());
    drop(tier2_data);
    let tier2_hint = Bytes::from(tier2.take_hint_bytes());
    eprintln!("  Tier 2 YPIR ready (hint: {} bytes)", tier2_hint.len());

    eprintln!("Server ready in {:.1}s", t_total.elapsed().as_secs_f64());

    Ok(ServingState {
        tier0_data,
        tier1,
        tier2,
        tier1_scenario,
        tier2_scenario,
        tier1_hint,
        tier2_hint,
        metadata,
    })
}

// ── Snapshot management endpoints ─────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub(crate) struct PrepareRequest {
    height: u64,
}

async fn check_active_round(chain_url: &str) -> Result<Option<String>> {
    let url = format!("{}/shielded-vote/v1/rounds/active", chain_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client.get(&url).send().await?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let body: serde_json::Value = resp.json().await?;
    if let Some(round) = body.get("round") {
        if round.is_object() && !round.is_null() {
            let round_id = round
                .get("vote_round_id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            return Ok(Some(round_id));
        }
    }
    Ok(None)
}

pub(crate) async fn post_snapshot_prepare(
    State(state): State<Arc<AppState>>,
    axum::Json(req): axum::Json<PrepareRequest>,
) -> impl IntoResponse {
    let height = req.height;

    if height < sync_nullifiers::NU5_ACTIVATION_HEIGHT {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": format!(
                    "height {} is below NU5 activation ({})",
                    height,
                    sync_nullifiers::NU5_ACTIVATION_HEIGHT
                )
            })),
        )
            .into_response();
    }
    if height % 10 != 0 {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": format!("height {} must be a multiple of 10", height)
            })),
        )
            .into_response();
    }

    let rebuild_guard = match Arc::clone(&state.rebuild_lock).try_lock_owned() {
        Ok(guard) => guard,
        Err(_) => {
            let phase = state.phase.read().await;
            return (
                StatusCode::CONFLICT,
                axum::Json(serde_json::json!({
                    "error": "rebuild already in progress",
                    "current": *phase,
                })),
            )
                .into_response();
        }
    };

    {
        let lwd_url = state.lwd_urls.first().cloned().unwrap_or_default();
        if !lwd_url.is_empty() {
            match sync_nullifiers::fetch_chain_tip(&lwd_url).await {
                Ok(tip) => {
                    if height > tip {
                        return (
                            StatusCode::BAD_REQUEST,
                            axum::Json(serde_json::json!({
                                "error": format!("height {} exceeds chain tip ({})", height, tip)
                            })),
                        )
                            .into_response();
                    }
                }
                Err(e) => {
                    warn!(error = %e, "failed to fetch chain tip, skipping validation");
                }
            }
        }
    }

    if let Some(chain_url) = &state.chain_url {
        match check_active_round(chain_url).await {
            Ok(Some(round_id)) => {
                return (
                    StatusCode::CONFLICT,
                    axum::Json(serde_json::json!({
                        "error": "cannot rebuild while round is active",
                        "round_id": round_id,
                    })),
                )
                    .into_response();
            }
            Ok(None) => {}
            Err(e) => {
                warn!(error = %e, "failed to check active round, proceeding anyway");
            }
        }
    }

    {
        let mut phase = state.phase.write().await;
        *phase = ServerPhase::Rebuilding {
            target_height: height,
            progress: "starting".to_string(),
            progress_pct: 0,
        };
    }

    let state_clone = Arc::clone(&state);
    tokio::task::spawn(async move {
        let _rebuild_guard = rebuild_guard;
        let result = run_rebuild(state_clone.clone(), height).await;
        if let Err(e) = result {
            let msg = format!("{:?}", e);
            warn!(error = %msg, "rebuild failed");
            let mut phase = state_clone.phase.write().await;
            *phase = ServerPhase::Error { message: msg };
        }
    });

    (
        StatusCode::ACCEPTED,
        axum::Json(serde_json::json!({
            "status": "rebuilding",
            "target_height": height,
        })),
    )
        .into_response()
}

/// Run the full rebuild pipeline: ingest (if needed) → export → load.
async fn run_rebuild(state: Arc<AppState>, target_height: u64) -> Result<()> {
    let data_dir = state.data_dir.clone();
    let pir_data_dir = state.pir_data_dir.clone();
    let lwd_urls = state.lwd_urls.clone();

    {
        let mut phase = state.phase.write().await;
        *phase = ServerPhase::Rebuilding {
            target_height,
            progress: "checking sync state".to_string(),
            progress_pct: 0,
        };
    }

    let current_height = file_store::load_checkpoint(&data_dir)?
        .map(|(h, _)| h)
        .unwrap_or(0);

    if target_height > current_height {
        {
            let mut phase = state.phase.write().await;
            *phase = ServerPhase::Rebuilding {
                target_height,
                progress: format!("ingesting blocks {current_height}..{target_height}"),
                progress_pct: 2,
            };
        }

        let dd = data_dir.clone();
        let lwd = lwd_urls.clone();
        let state_ref = Arc::clone(&state);
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(sync_nullifiers::sync(&dd, &lwd, Some(target_height), |h, t, _, _| {
                eprintln!("  ingest: {h}/{t}");
                let pct = if t > 0 {
                    2 + ((h as f64 / t as f64) * 8.0) as u8
                } else {
                    5
                };
                if let Ok(mut phase) = state_ref.phase.try_write() {
                    *phase = ServerPhase::Rebuilding {
                        target_height,
                        progress: format!("ingesting {h}/{t}"),
                        progress_pct: pct,
                    };
                }
            }))?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
    }

    {
        let mut phase = state.phase.write().await;
        *phase = ServerPhase::Rebuilding {
            target_height,
            progress: "loading nullifiers".to_string(),
            progress_pct: 10,
        };
    }

    let dd = data_dir.clone();
    let pd = pir_data_dir.clone();
    let state_ref = Arc::clone(&state);
    tokio::task::spawn_blocking(move || {
        let entry = file_store::offset_for_height(&dd, target_height)?;
        let (idx_height, byte_offset) = entry.ok_or_else(|| {
            anyhow::anyhow!("no index entry for target height {}", target_height)
        })?;
        eprintln!(
            "  Export: loading nullifiers up to height {} (offset={})",
            idx_height, byte_offset
        );
        let nfs = file_store::load_nullifiers_up_to(&dd, byte_offset)?;
        eprintln!("  Loaded {} nullifiers", nfs.len());

        pir_export::build_and_export_with_progress(nfs, &pd, Some(idx_height), |msg, pct| {
            let overall_pct = 10 + (pct as u16 * 45 / 55).min(45) as u8;
            if let Ok(mut phase) = state_ref.phase.try_write() {
                *phase = ServerPhase::Rebuilding {
                    target_height,
                    progress: msg.to_string(),
                    progress_pct: overall_pct,
                };
            }
        })?;
        Ok::<_, anyhow::Error>(())
    })
    .await??;

    {
        let mut phase = state.phase.write().await;
        *phase = ServerPhase::Rebuilding {
            target_height,
            progress: "loading YPIR servers".to_string(),
            progress_pct: 60,
        };
    }

    let pd = pir_data_dir.clone();
    let new_serving = tokio::task::spawn_blocking(move || load_serving_state(&pd)).await??;

    {
        let mut serving = state.serving.write().await;
        *serving = Some(new_serving);
    }
    {
        let mut phase = state.phase.write().await;
        *phase = ServerPhase::Serving;
    }

    info!(target_height, "rebuild complete");
    Ok(())
}

pub(crate) async fn get_snapshot_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let (phase_json, height, num_ranges) = {
        let phase = state.phase.read().await;
        let serving = state.serving.read().await;
        let h = serving.as_ref().and_then(|s| s.metadata.height);
        let n = serving.as_ref().map(|s| s.metadata.num_ranges);
        (serde_json::to_value(&*phase).unwrap_or_default(), h, n)
    };

    let zcash_tip = if let Some(lwd_url) = state.lwd_urls.first() {
        sync_nullifiers::fetch_chain_tip(lwd_url).await.ok()
    } else {
        None
    };

    let mut resp = phase_json;
    if let Some(obj) = resp.as_object_mut() {
        obj.insert("height".to_string(), serde_json::json!(height));
        obj.insert("num_ranges".to_string(), serde_json::json!(num_ranges));
        obj.insert("zcash_tip".to_string(), serde_json::json!(zcash_tip));
    }

    axum::Json(resp)
}
