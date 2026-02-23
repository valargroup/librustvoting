//! YPIR+SP server wrapper and shared types for the PIR HTTP server.
//!
//! This module encapsulates all YPIR operations, providing a clean interface
//! that both the HTTP server (`main.rs`) and the test harness (`pir-test`)
//! can use.

use std::time::Instant;
use serde::{Deserialize, Serialize};

use ypir::params::params_for_scenario_simplepir;
use ypir::server::{OfflinePrecomputedValues, YServer};

// Re-export constants from pir-export for convenience.
pub use pir_export::{
    TIER1_ITEM_BITS, TIER1_ROWS, TIER1_ROW_BYTES, TIER2_ITEM_BITS, TIER2_ROWS, TIER2_ROW_BYTES,
};

// ── YPIR scenario params ─────────────────────────────────────────────────────

/// Parameters needed for a YPIR scenario. Serialized over HTTP so the client
/// can reconstruct matching params locally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YpirScenario {
    pub num_items: usize,
    pub item_size_bits: usize,
}

/// Tier 1 YPIR scenario.
pub fn tier1_scenario() -> YpirScenario {
    YpirScenario {
        num_items: TIER1_ROWS,
        item_size_bits: TIER1_ITEM_BITS,
    }
}

/// Tier 2 YPIR scenario.
pub fn tier2_scenario() -> YpirScenario {
    YpirScenario {
        num_items: TIER2_ROWS,
        item_size_bits: TIER2_ITEM_BITS,
    }
}

// ── PIR server state ─────────────────────────────────────────────────────────

/// Holds the YPIR server state for one tier.
///
/// Wraps the YPIR `YServer` and its offline precomputed values. Answers
/// individual queries via `answer_query`.
pub struct TierServer<'a> {
    server: YServer<'a, u8>,
    offline: OfflinePrecomputedValues<'a>,
    scenario: YpirScenario,
}

impl<'a> TierServer<'a> {
    /// Initialize a YPIR+SP server from raw tier data.
    ///
    /// `data` is the flat binary tier file (rows × row_bytes).
    /// This performs the expensive offline precomputation.
    pub fn new(data: &[u8], scenario: YpirScenario) -> Self {
        let t0 = Instant::now();
        let params = params_for_scenario_simplepir(scenario.num_items, scenario.item_size_bits);

        eprintln!(
            "  YPIR server init: {} items × {} bits",
            scenario.num_items, scenario.item_size_bits
        );

        let server = YServer::<u8>::new(&params, data.iter(), true, false, true);

        let t1 = Instant::now();
        eprintln!(
            "  YPIR server constructed in {:.1}s",
            (t1 - t0).as_secs_f64()
        );

        let offline = server.perform_offline_precomputation_simplepir(None);
        eprintln!(
            "  YPIR offline precomputation done in {:.1}s",
            t1.elapsed().as_secs_f64()
        );

        Self {
            server,
            offline,
            scenario,
        }
    }

    /// Answer a single YPIR+SP query.
    ///
    /// The query bytes must be in the length-prefixed format:
    /// `[8 bytes: packed_query_row byte length as LE u64][packed_query_row bytes][pub_params bytes]`
    ///
    /// Returns the serialized response as LE u64 bytes.
    pub fn answer_query(&mut self, query_bytes: &[u8]) -> Vec<u8> {
        // Parse length-prefixed format: [8: pqr_byte_len][pqr][pub_params]
        let pqr_byte_len =
            u64::from_le_bytes(query_bytes[..8].try_into().unwrap()) as usize;

        let pqr: Vec<u64> = query_bytes[8..8 + pqr_byte_len]
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();

        let pub_params: Vec<u64> = query_bytes[8 + pqr_byte_len..]
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();

        // Run the YPIR online computation
        let response = self.server.perform_online_computation_simplepir(
            &pqr,
            &self.offline,
            &[&pub_params],
            None,
        );

        // Serialize response as LE u64 bytes
        response
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect()
    }

    pub fn scenario(&self) -> &YpirScenario {
        &self.scenario
    }

    /// Return the SimplePIR hint (hint_0) that the client needs.
    ///
    /// Serialized as LE u64 bytes.
    pub fn hint_bytes(&self) -> Vec<u8> {
        self.offline
            .hint_0
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect()
    }
}

// ── Root info ────────────────────────────────────────────────────────────────

/// Root and metadata returned by GET /root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootInfo {
    pub root29: String,
    pub root26: String,
    pub num_ranges: usize,
    pub pir_depth: usize,
    pub height: Option<u64>,
}

/// Health check response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthInfo {
    pub status: String,
    pub tier1_rows: usize,
    pub tier2_rows: usize,
    pub tier1_row_bytes: usize,
    pub tier2_row_bytes: usize,
}
