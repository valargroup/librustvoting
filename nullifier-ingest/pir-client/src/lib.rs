//! PIR client library for private Merkle path retrieval.
//!
//! Provides [`PirClient`] which connects to a `pir-server` instance and
//! retrieves circuit-ready `ImtProofData` without revealing the queried
//! nullifier to the server.

use std::time::Instant;

use anyhow::{Context, Result};
use ff::PrimeField as _;
use pasta_curves::Fp;
use serde::{Deserialize, Serialize};

use imt_tree::hasher::PoseidonHasher;
use imt_tree::tree::{precompute_empty_hashes, TREE_DEPTH};
// Re-exported so downstream crates (e.g. librustvoting) can reference the type
// returned by PirClientBlocking::fetch_proof without a direct imt-tree dependency.
pub use imt_tree::ImtProofData;

use pir_export::tier0::Tier0Data;
use pir_export::tier1::Tier1Row;
use pir_export::tier2::Tier2Row;
use pir_export::{
    PIR_DEPTH, TIER0_LAYERS, TIER1_LAYERS, TIER1_LEAVES, TIER1_ROW_BYTES, TIER2_LEAVES,
    TIER2_ROW_BYTES,
};

use ypir::client::YPIRClient;

// ── Shared types (duplicated from pir-server to avoid feature unification) ───

/// Parameters needed for a YPIR scenario. Serialized over HTTP so the client
/// can reconstruct matching params locally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YpirScenario {
    pub num_items: usize,
    pub item_size_bits: usize,
}

/// Root and metadata returned by GET /root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootInfo {
    pub root29: String,
    pub root26: String,
    pub num_ranges: usize,
    pub pir_depth: usize,
    pub height: Option<u64>,
}

// ── Timing breakdown ─────────────────────────────────────────────────────────

/// Per-tier timing breakdown for a single YPIR query.
struct TierTiming {
    gen_ms: f64, // client-side query generation
    upload_bytes: usize,
    download_bytes: usize,
    rtt_ms: f64,    // upload + server compute + download (wall clock)
    decode_ms: f64, // client-side response decoding
    server_req_id: Option<u64>,
    server_total_ms: Option<f64>,
    server_validate_ms: Option<f64>,
    server_decode_copy_ms: Option<f64>,
    server_compute_ms: Option<f64>,
    net_queue_ms: Option<f64>,
    upload_to_server_ms: Option<f64>,
    download_from_server_ms: f64,
}

/// Per-note timing breakdown covering both tiers.
struct NoteTiming {
    tier1: TierTiming,
    tier2: TierTiming,
    total_ms: f64,
}

// ── HTTP-based PIR client ────────────────────────────────────────────────────

/// PIR client that connects to a `pir-server` instance over HTTP.
///
/// Downloads Tier 0 data and YPIR parameters during `connect()`, then
/// performs private queries via `fetch_proof()`.
pub struct PirClient {
    server_url: String,
    http: reqwest::Client,
    tier0: Tier0Data,
    tier1_scenario: YpirScenario,
    tier2_scenario: YpirScenario,
    num_ranges: usize,
    empty_hashes: [Fp; TREE_DEPTH],
    root29: Fp,
}

#[inline]
fn valid_leaves_for_row(num_ranges: usize, row_idx: usize) -> usize {
    let row_start = row_idx.saturating_mul(TIER2_LEAVES);
    num_ranges.saturating_sub(row_start).min(TIER2_LEAVES)
}

impl PirClient {
    /// Connect to a PIR server, downloading Tier 0 data and YPIR parameters.
    pub async fn connect(server_url: &str) -> Result<Self> {
        let http = reqwest::Client::new();
        let base = server_url.trim_end_matches('/');

        // Download Tier 0 data, YPIR params, and root concurrently
        let t0 = Instant::now();
        let (tier0_resp, tier1_resp, tier2_resp, root_resp) = tokio::try_join!(
            http.get(format!("{base}/tier0")).send(),
            http.get(format!("{base}/params/tier1")).send(),
            http.get(format!("{base}/params/tier2")).send(),
            http.get(format!("{base}/root")).send(),
        )
        .map_err(|e| anyhow::anyhow!("connect fetch failed: {e}"))?;

        let tier0_bytes = tier0_resp.error_for_status()?.bytes().await?;
        eprintln!(
            "  Downloaded Tier 0: {} bytes in {:.1}s",
            tier0_bytes.len(),
            t0.elapsed().as_secs_f64()
        );
        let tier0 = Tier0Data::from_bytes(tier0_bytes.to_vec())?;

        let tier1_scenario: YpirScenario = tier1_resp
            .error_for_status()
            .context("GET /params/tier1 failed")?
            .json()
            .await?;
        let tier2_scenario: YpirScenario = tier2_resp
            .error_for_status()
            .context("GET /params/tier2 failed")?
            .json()
            .await?;

        let root_info: RootInfo = root_resp
            .error_for_status()
            .context("GET /root failed")?
            .json()
            .await?;
        anyhow::ensure!(
            root_info.pir_depth == PIR_DEPTH,
            "server pir_depth {} != expected {}",
            root_info.pir_depth,
            PIR_DEPTH
        );
        let root29_bytes = hex::decode(&root_info.root29)?;
        anyhow::ensure!(
            root29_bytes.len() == 32,
            "root29 hex decoded to {} bytes, expected 32",
            root29_bytes.len()
        );
        let mut root29_arr = [0u8; 32];
        root29_arr.copy_from_slice(&root29_bytes);
        let root29 = Option::from(Fp::from_repr(root29_arr))
            .ok_or_else(|| anyhow::anyhow!("invalid root29 field element"))?;

        let empty_hashes = precompute_empty_hashes();

        Ok(Self {
            server_url: base.to_string(),
            http,
            tier0,
            tier1_scenario,
            tier2_scenario,
            num_ranges: root_info.num_ranges,
            empty_hashes,
            root29,
        })
    }

    /// Perform private Merkle path retrieval for a nullifier.
    ///
    /// Returns circuit-ready `ImtProofData` with a 29-element path
    /// (26 PIR siblings + 3 empty-hash padding).
    pub async fn fetch_proof(&self, nullifier: Fp) -> Result<ImtProofData> {
        let (proof, _timing) = self.fetch_proof_timed(nullifier).await?;
        Ok(proof)
    }

    /// Internal: fetch proof and return timing breakdown.
    async fn fetch_proof_timed(&self, nullifier: Fp) -> Result<(ImtProofData, NoteTiming)> {
        let note_start = Instant::now();
        let mut path = [Fp::default(); TREE_DEPTH]; // 29 siblings
        let hasher = PoseidonHasher::new();

        // ── Tier 0: plaintext lookup ─────────────────────────────────────
        let s1 = self
            .tier0
            .find_subtree(nullifier)
            .context("nullifier not found in any Tier 0 subtree")?;

        // Extract 11 siblings from Tier 0 (bottom-up levels 15..25)
        let tier0_siblings = self.tier0.extract_siblings(s1);
        for (i, &sib) in tier0_siblings.iter().enumerate() {
            path[PIR_DEPTH - TIER0_LAYERS + i] = sib; // path[15..26]
        }

        // ── Tier 1: YPIR query for row s1 ────────────────────────────────
        let (tier1_row, tier1_timing) = self.ypir_query_tier1(s1).await?;
        let tier1 = Tier1Row::from_bytes(&tier1_row)?;

        let s2 = tier1
            .find_sub_subtree(nullifier)
            .context("nullifier not found in any Tier 1 sub-subtree")?;

        // Extract 8 siblings from Tier 1 (bottom-up levels 7..14)
        let tier1_siblings = tier1.extract_siblings(s2);
        for (i, &sib) in tier1_siblings.iter().enumerate() {
            path[PIR_DEPTH - TIER0_LAYERS - TIER1_LAYERS + i] = sib; // path[7..15]
        }

        // ── Tier 2: YPIR query for row (s1 * 256 + s2) ──────────────────
        let t2_row_idx = s1 * TIER1_LEAVES + s2;
        let (tier2_row, tier2_timing) = self.ypir_query_tier2(t2_row_idx).await?;
        let tier2 = Tier2Row::from_bytes(&tier2_row)?;
        let valid_leaves = valid_leaves_for_row(self.num_ranges, t2_row_idx);

        let leaf_local_idx = tier2
            .find_leaf(nullifier, valid_leaves)
            .context("nullifier not found in Tier 2 leaf scan")?;

        // Extract 7 siblings from Tier 2 (bottom-up levels 0..6)
        let tier2_siblings = tier2.extract_siblings(leaf_local_idx, valid_leaves, &hasher);
        for (i, &sib) in tier2_siblings.iter().enumerate() {
            path[i] = sib; // path[0..7]
        }

        // ── Path padding (depth 26 → 29) ────────────────────────────────
        for level in PIR_DEPTH..TREE_DEPTH {
            path[level] = self.empty_hashes[level];
        }

        // ── Compute leaf position and range data ─────────────────────────
        let global_leaf_idx = t2_row_idx * TIER2_LEAVES + leaf_local_idx;
        let (low, width) = tier2.leaf_record(leaf_local_idx);

        let total_ms = note_start.elapsed().as_secs_f64() * 1000.0;

        let proof = ImtProofData {
            root: self.root29,
            low,
            width,
            leaf_pos: global_leaf_idx as u32,
            path,
        };
        let timing = NoteTiming {
            tier1: tier1_timing,
            tier2: tier2_timing,
            total_ms,
        };
        Ok((proof, timing))
    }

    /// Perform private Merkle path retrieval for multiple nullifiers in parallel.
    ///
    /// All queries run concurrently via `try_join_all`, sharing the same
    /// `PirClient` (and thus the same HTTP client and Tier 0 data).
    pub async fn fetch_proofs(&self, nullifiers: &[Fp]) -> Result<Vec<ImtProofData>> {
        eprintln!(
            "[PIR] Starting parallel fetch for {} notes...",
            nullifiers.len()
        );
        let wall_start = Instant::now();

        let futures: Vec<_> = nullifiers
            .iter()
            .enumerate()
            .map(|(i, &nf)| async move {
                let (proof, timing) = self.fetch_proof_timed(nf).await?;
                Ok::<_, anyhow::Error>((i, proof, timing))
            })
            .collect();

        let results_with_timing = futures::future::try_join_all(futures).await?;
        let wall_ms = wall_start.elapsed().as_secs_f64() * 1000.0;

        // Print timing table
        // gen = client-side YPIR query generation
        // network = upload query + server compute + download response
        // decode = client-side YPIR response decryption
        fn fmt_time(ms: f64) -> String {
            if ms >= 1000.0 {
                format!("{:>5.1}s ", ms / 1000.0)
            } else {
                format!("{:>5.0}ms", ms)
            }
        }
        fn fmt_opt_time(ms: Option<f64>) -> String {
            match ms {
                Some(v) => fmt_time(v),
                None => "  n/a ".to_string(),
            }
        }
        eprintln!("[PIR] ┌─────┬──────────┬─────────────┬──────────┬──────────┬─────────────┬──────────┬────────┐");
        eprintln!("[PIR] │ Note│ T1 keygen│ T1 upload+  │ T1 decode│ T2 keygen│ T2 upload+  │ T2 decode│ Total  │");
        eprintln!("[PIR] │     │ (client) │ server+down │ (client) │ (client) │ server+down │ (client) │        │");
        eprintln!("[PIR] ├─────┼──────────┼─────────────┼──────────┼──────────┼─────────────┼──────────┼────────┤");
        for &(i, _, ref t) in &results_with_timing {
            eprintln!(
                "[PIR] │  {i:>2} │  {:>6} │   {:>7}   │  {:>6} │  {:>6} │   {:>7}   │  {:>6} │{} │",
                fmt_time(t.tier1.gen_ms),
                fmt_time(t.tier1.rtt_ms),
                fmt_time(t.tier1.decode_ms),
                fmt_time(t.tier2.gen_ms),
                fmt_time(t.tier2.rtt_ms),
                fmt_time(t.tier2.decode_ms),
                fmt_time(t.total_ms),
            );
        }
        eprintln!("[PIR] └─────┴──────────┴─────────────┴──────────┴──────────┴─────────────┴──────────┴────────┘");
        for &(i, _, ref t) in &results_with_timing {
            eprintln!(
                "[PIR] Note {i:>2} transfer: T1 up={:.0}KB down={:.0}KB | T2 up={:.1}MB down={:.0}KB",
                t.tier1.upload_bytes as f64 / 1024.0,
                t.tier1.download_bytes as f64 / 1024.0,
                t.tier2.upload_bytes as f64 / (1024.0 * 1024.0),
                t.tier2.download_bytes as f64 / 1024.0,
            );
            eprintln!(
                "[PIR] Note {i:>2} server/net: T1 {} / {} | T2 {} / {}",
                fmt_opt_time(t.tier1.server_total_ms),
                fmt_opt_time(t.tier1.net_queue_ms),
                fmt_opt_time(t.tier2.server_total_ms),
                fmt_opt_time(t.tier2.net_queue_ms),
            );
            eprintln!(
                "[PIR] Note {i:>2} up/srv/down: T1 {} / {} / {} | T2 {} / {} / {}",
                fmt_opt_time(t.tier1.upload_to_server_ms),
                fmt_opt_time(t.tier1.server_total_ms),
                fmt_time(t.tier1.download_from_server_ms),
                fmt_opt_time(t.tier2.upload_to_server_ms),
                fmt_opt_time(t.tier2.server_total_ms),
                fmt_time(t.tier2.download_from_server_ms),
            );
            eprintln!(
                "[PIR] Note {i:>2} server stages: T1(v={} copy={} compute={}) T2(v={} copy={} compute={})",
                fmt_opt_time(t.tier1.server_validate_ms),
                fmt_opt_time(t.tier1.server_decode_copy_ms),
                fmt_opt_time(t.tier1.server_compute_ms),
                fmt_opt_time(t.tier2.server_validate_ms),
                fmt_opt_time(t.tier2.server_decode_copy_ms),
                fmt_opt_time(t.tier2.server_compute_ms),
            );
            eprintln!(
                "[PIR] Note {i:>2} req ids: T1={:?} T2={:?}",
                t.tier1.server_req_id, t.tier2.server_req_id
            );
        }
        eprintln!(
            "[PIR] Upload per note: T1={:.0}KB T2={:.1}MB  |  Wall clock: {:.2}s",
            results_with_timing
                .first()
                .map(|(_, _, t)| t.tier1.upload_bytes)
                .unwrap_or(0) as f64
                / 1024.0,
            results_with_timing
                .first()
                .map(|(_, _, t)| t.tier2.upload_bytes)
                .unwrap_or(0) as f64
                / (1024.0 * 1024.0),
            wall_ms / 1000.0,
        );

        let proofs = results_with_timing
            .into_iter()
            .map(|(_, proof, _)| proof)
            .collect();
        Ok(proofs)
    }

    /// Send a YPIR query for a Tier 1 row and return the decrypted row bytes.
    async fn ypir_query_tier1(&self, row_idx: usize) -> Result<(Vec<u8>, TierTiming)> {
        anyhow::ensure!(
            row_idx < self.tier1_scenario.num_items,
            "tier1 row_idx {} >= num_items {}",
            row_idx,
            self.tier1_scenario.num_items
        );
        let t0 = Instant::now();
        let ypir_client = YPIRClient::from_db_sz(
            self.tier1_scenario.num_items as u64,
            self.tier1_scenario.item_size_bits as u64,
            true,
        );

        // Generate encrypted YPIR query
        let (query, seed) = ypir_client.generate_query_simplepir(row_idx);
        let gen_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // Serialize with length prefix: [8: pqr_byte_len][pqr][pub_params]
        let pqr = query.0.as_slice();
        let pp = query.1.as_slice();
        let pqr_byte_len = pqr.len() * 8;
        let mut payload = Vec::with_capacity(8 + (pqr.len() + pp.len()) * 8);
        payload.extend_from_slice(&(pqr_byte_len as u64).to_le_bytes());
        for &v in pqr {
            payload.extend_from_slice(&v.to_le_bytes());
        }
        for &v in pp {
            payload.extend_from_slice(&v.to_le_bytes());
        }
        let upload_bytes = payload.len();

        // Send to server
        let t1 = Instant::now();
        let url = format!("{}/tier1/query", self.server_url);
        let send_result = self.http.post(&url).body(payload).send().await;
        let send_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let resp = match send_result {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  YPIR tier1 send error: {:?}", e);
                return Err(e.into());
            }
        };
        let server_req_id = parse_header_u64(resp.headers(), "x-pir-req-id");
        let server_total_ms = parse_header_f64(resp.headers(), "x-pir-server-total-ms");
        let server_validate_ms = parse_header_f64(resp.headers(), "x-pir-server-validate-ms");
        let server_decode_copy_ms = parse_header_f64(resp.headers(), "x-pir-server-decode-copy-ms");
        let server_compute_ms = parse_header_f64(resp.headers(), "x-pir-server-compute-ms");
        let status = resp.status();
        let response_bytes = resp.bytes().await?;
        if !status.is_success() {
            anyhow::bail!(
                "tier1 query failed: HTTP {} body={}",
                status,
                String::from_utf8_lossy(&response_bytes)
            );
        }
        let rtt_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let download_from_server_ms = (rtt_ms - send_ms).max(0.0);
        let net_queue_ms = server_total_ms.map(|server_ms| (rtt_ms - server_ms).max(0.0));
        let upload_to_server_ms = server_total_ms.map(|server_ms| {
            // Approximate upload+request-path time as time-to-headers minus server compute.
            (send_ms - server_ms).max(0.0)
        });

        // Decode YPIR response to get the plaintext row
        let t2 = Instant::now();
        let decoded = ypir_client.decode_response_simplepir(seed, &response_bytes);
        let decode_ms = t2.elapsed().as_secs_f64() * 1000.0;

        anyhow::ensure!(
            decoded.len() >= TIER1_ROW_BYTES,
            "tier1 decoded response too short: {} bytes, expected >= {}",
            decoded.len(),
            TIER1_ROW_BYTES
        );
        Ok((
            decoded[..TIER1_ROW_BYTES].to_vec(),
            TierTiming {
                gen_ms,
                upload_bytes,
                download_bytes: response_bytes.len(),
                rtt_ms,
                decode_ms,
                server_req_id,
                server_total_ms,
                server_validate_ms,
                server_decode_copy_ms,
                server_compute_ms,
                net_queue_ms,
                upload_to_server_ms,
                download_from_server_ms,
            },
        ))
    }

    /// Send a YPIR query for a Tier 2 row and return the decrypted row bytes.
    async fn ypir_query_tier2(&self, row_idx: usize) -> Result<(Vec<u8>, TierTiming)> {
        anyhow::ensure!(
            row_idx < self.tier2_scenario.num_items,
            "tier2 row_idx {} >= num_items {}",
            row_idx,
            self.tier2_scenario.num_items
        );
        let t0 = Instant::now();
        let ypir_client = YPIRClient::from_db_sz(
            self.tier2_scenario.num_items as u64,
            self.tier2_scenario.item_size_bits as u64,
            true,
        );

        // Generate encrypted YPIR query
        let (query, seed) = ypir_client.generate_query_simplepir(row_idx);
        let gen_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // Serialize with length prefix: [8: pqr_byte_len][pqr][pub_params]
        let pqr = query.0.as_slice();
        let pp = query.1.as_slice();
        let pqr_byte_len = pqr.len() * 8;
        let mut payload = Vec::with_capacity(8 + (pqr.len() + pp.len()) * 8);
        payload.extend_from_slice(&(pqr_byte_len as u64).to_le_bytes());
        for &v in pqr {
            payload.extend_from_slice(&v.to_le_bytes());
        }
        for &v in pp {
            payload.extend_from_slice(&v.to_le_bytes());
        }
        let upload_bytes = payload.len();

        // Send to server
        let t1 = Instant::now();
        let url = format!("{}/tier2/query", self.server_url);
        let send_result = self.http.post(&url).body(payload).send().await;
        let send_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let resp = match send_result {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  YPIR tier2 send error: {:?}", e);
                return Err(e.into());
            }
        };
        let server_req_id = parse_header_u64(resp.headers(), "x-pir-req-id");
        let server_total_ms = parse_header_f64(resp.headers(), "x-pir-server-total-ms");
        let server_validate_ms = parse_header_f64(resp.headers(), "x-pir-server-validate-ms");
        let server_decode_copy_ms = parse_header_f64(resp.headers(), "x-pir-server-decode-copy-ms");
        let server_compute_ms = parse_header_f64(resp.headers(), "x-pir-server-compute-ms");
        let status = resp.status();
        let response_bytes = resp.bytes().await?;
        if !status.is_success() {
            anyhow::bail!(
                "tier2 query failed: HTTP {} body={}",
                status,
                String::from_utf8_lossy(&response_bytes)
            );
        }
        let rtt_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let download_from_server_ms = (rtt_ms - send_ms).max(0.0);
        let net_queue_ms = server_total_ms.map(|server_ms| (rtt_ms - server_ms).max(0.0));
        let upload_to_server_ms = server_total_ms.map(|server_ms| {
            // Approximate upload+request-path time as time-to-headers minus server compute.
            (send_ms - server_ms).max(0.0)
        });

        // Decode YPIR response to get the plaintext row
        let t2 = Instant::now();
        let decoded = ypir_client.decode_response_simplepir(seed, &response_bytes);
        let decode_ms = t2.elapsed().as_secs_f64() * 1000.0;

        anyhow::ensure!(
            decoded.len() >= TIER2_ROW_BYTES,
            "tier2 decoded response too short: {} bytes, expected >= {}",
            decoded.len(),
            TIER2_ROW_BYTES
        );
        Ok((
            decoded[..TIER2_ROW_BYTES].to_vec(),
            TierTiming {
                gen_ms,
                upload_bytes,
                download_bytes: response_bytes.len(),
                rtt_ms,
                decode_ms,
                server_req_id,
                server_total_ms,
                server_validate_ms,
                server_decode_copy_ms,
                server_compute_ms,
                net_queue_ms,
                upload_to_server_ms,
                download_from_server_ms,
            },
        ))
    }
}

fn parse_header_f64(headers: &reqwest::header::HeaderMap, name: &'static str) -> Option<f64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
}

fn parse_header_u64(headers: &reqwest::header::HeaderMap, name: &'static str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

// ── Blocking wrapper ─────────────────────────────────────────────────────────

/// Synchronous wrapper around [`PirClient`] for use from non-async code.
///
/// Owns a Tokio runtime internally so callers (e.g. librustvoting, which must
/// stay synchronous for the Halo2 prover) don't need to manage one.
pub struct PirClientBlocking {
    inner: PirClient,
    rt: tokio::runtime::Runtime,
}

impl PirClientBlocking {
    /// Connect to a PIR server (blocking). Downloads Tier 0 data and YPIR params.
    pub fn connect(server_url: &str) -> Result<Self> {
        let rt = tokio::runtime::Runtime::new()?;
        let inner = rt.block_on(PirClient::connect(server_url))?;
        Ok(Self { inner, rt })
    }

    /// Perform a private Merkle path retrieval for a nullifier (blocking).
    pub fn fetch_proof(&self, nullifier: Fp) -> Result<ImtProofData> {
        self.rt.block_on(self.inner.fetch_proof(nullifier))
    }

    /// Perform private Merkle path retrieval for multiple nullifiers in parallel (blocking).
    pub fn fetch_proofs(&self, nullifiers: &[Fp]) -> Result<Vec<ImtProofData>> {
        self.rt.block_on(self.inner.fetch_proofs(nullifiers))
    }

    /// The depth-29 root (PIR depth 26 padded to tree depth 29).
    pub fn root29(&self) -> Fp {
        self.inner.root29
    }
}

// ── Local (in-process) PIR client ────────────────────────────────────────────

/// Perform a complete local PIR proof retrieval without HTTP.
///
/// This is used by `pir-test local` mode. It takes the tier data directly
/// (as built by `pir-export`) and performs the YPIR operations in-process.
pub fn fetch_proof_local(
    tier0_data: &[u8],
    tier1_data: &[u8],
    tier2_data: &[u8],
    num_ranges: usize,
    nullifier: Fp,
    empty_hashes: &[Fp; TREE_DEPTH],
    root29: Fp,
) -> Result<ImtProofData> {
    let mut path = [Fp::default(); TREE_DEPTH];
    let hasher = PoseidonHasher::new();
    let tier0 = Tier0Data::from_bytes(tier0_data.to_vec())?;

    // ── Tier 0: plaintext lookup ─────────────────────────────────────────
    let s1 = tier0
        .find_subtree(nullifier)
        .context("nullifier not found in any Tier 0 subtree")?;

    let tier0_siblings = tier0.extract_siblings(s1);
    for (i, &sib) in tier0_siblings.iter().enumerate() {
        path[PIR_DEPTH - TIER0_LAYERS + i] = sib;
    }

    // ── Tier 1: direct row lookup (no YPIR in local mode) ────────────────
    let t1_offset = s1 * TIER1_ROW_BYTES;
    anyhow::ensure!(
        t1_offset + TIER1_ROW_BYTES <= tier1_data.len(),
        "tier1 data too short: need {} bytes at offset {}, have {}",
        TIER1_ROW_BYTES,
        t1_offset,
        tier1_data.len()
    );
    let tier1_row = &tier1_data[t1_offset..t1_offset + TIER1_ROW_BYTES];
    let tier1 = Tier1Row::from_bytes(tier1_row)?;

    let s2 = tier1
        .find_sub_subtree(nullifier)
        .context("nullifier not found in any Tier 1 sub-subtree")?;

    let tier1_siblings = tier1.extract_siblings(s2);
    for (i, &sib) in tier1_siblings.iter().enumerate() {
        path[PIR_DEPTH - TIER0_LAYERS - TIER1_LAYERS + i] = sib;
    }

    // ── Tier 2: direct row lookup (no YPIR in local mode) ────────────────
    let t2_row_idx = s1 * TIER1_LEAVES + s2;
    let t2_offset = t2_row_idx * TIER2_ROW_BYTES;
    anyhow::ensure!(
        t2_offset + TIER2_ROW_BYTES <= tier2_data.len(),
        "tier2 data too short: need {} bytes at offset {}, have {}",
        TIER2_ROW_BYTES,
        t2_offset,
        tier2_data.len()
    );
    let tier2_row = &tier2_data[t2_offset..t2_offset + TIER2_ROW_BYTES];
    let tier2 = Tier2Row::from_bytes(tier2_row)?;
    let valid_leaves = valid_leaves_for_row(num_ranges, t2_row_idx);

    let leaf_local_idx = tier2
        .find_leaf(nullifier, valid_leaves)
        .context("nullifier not found in Tier 2 leaf scan")?;

    let tier2_siblings = tier2.extract_siblings(leaf_local_idx, valid_leaves, &hasher);
    for (i, &sib) in tier2_siblings.iter().enumerate() {
        path[i] = sib;
    }

    // ── Path padding (depth 26 → 29) ────────────────────────────────────
    for level in PIR_DEPTH..TREE_DEPTH {
        path[level] = empty_hashes[level];
    }

    let global_leaf_idx = t2_row_idx * TIER2_LEAVES + leaf_local_idx;
    let (low, width) = tier2.leaf_record(leaf_local_idx);

    Ok(ImtProofData {
        root: root29,
        low,
        width,
        leaf_pos: global_leaf_idx as u32,
        path,
    })
}
