//! The published staging configuration, and which parts of it the suite reuses.
//!
//! Endpoints are not hardcoded here. `vote_servers`, `pir_endpoints`, and
//! `supported_versions` come from the same published stage document a staging
//! wallet reads, so an endpoint move or a version bump reaches this suite the
//! way it reaches a wallet, instead of drifting until a run fails for a reason
//! that looks like a recovery bug.
//!
//! # Why the round entries are not reused
//!
//! The published stage config carries both generations: as of 2026-09-05 it
//! holds 397 rounds, 245 at `auth_version: 1` and 152 at `auth_version: 2`,
//! the v2 ones signed by `valar-test-keplr-derived` (111) and
//! `zodl-stage-vote-manager` (41). This crate's `verify_round_entry` accepts
//! only `ROUND_AUTH_VERSION_V2`, so the v1 majority is unusable by a wallet
//! built from this branch; the v2 entries would authenticate.
//!
//! The suite still publishes its own entry rather than adopting one. It
//! provisions a round per mutative stage, so an entry has to be minted for a
//! round id that does not exist yet, and reusing a stranger's round would tie
//! the suite's assertions to a ballot and lifetime it does not control.
//! Entries are signed over `RoundAuthPayloadV2` — binding round id, `ea_pk`,
//! and the PIR layout — and served from a suite-owned dynamic config, the way
//! `zcash_voting/examples/pir_smoke.rs` does. Endpoints in that document come
//! from stage; only the round entry is ours.

use serde::{Deserialize, Serialize};

/// Published staging static config: the trust anchor a staging wallet pins.
pub const STAGE_STATIC_CONFIG_URL: &str =
    "https://voting.valargroup.org/stage/static-voting-config.json";

/// Published staging dynamic config, as named by the static config above.
pub const STAGE_DYNAMIC_CONFIG_URL: &str =
    "https://raw.githubusercontent.com/valargroup/token-holder-voting-config/main/stage/dynamic-voting-config.json";

/// One labelled service endpoint.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    #[serde(default)]
    pub label: String,
}

/// Protocol versions the deployment advertises.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SupportedVersions {
    #[serde(default)]
    pub pir: Vec<String>,
    #[serde(default)]
    pub vote_protocol: String,
    #[serde(default)]
    pub tally: String,
    #[serde(default)]
    pub vote_server: String,
}

/// PIR parameters the deployment advertises.
///
/// Carried because `RoundAuthPayloadV2` signs over the layout alongside the
/// round id and `ea_pk`. A round entry signed against a different layout than
/// the one the deployment publishes would fail verification in the wallet, so
/// this is not incidental metadata.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PirLayout {
    pub pir_depth: u32,
    pub tier0_layers: u32,
    pub tier1_layers: u32,
    pub poly_len: u32,
}

/// The parts of the stage dynamic config this suite reuses.
///
/// Round entries are deliberately not modelled. They are `auth_version: 1` and
/// this crate would reject them; the suite publishes its own v2 entry instead.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StageDeployment {
    pub config_version: u32,
    pub vote_servers: Vec<Endpoint>,
    #[serde(default)]
    pub pir_endpoints: Vec<Endpoint>,
    pub pir_layout: PirLayout,
    pub supported_versions: SupportedVersions,
}

/// Why the stage configuration could not be used.
#[derive(Debug)]
pub enum StageConfigError {
    Malformed(serde_json::Error),
    /// The document names no vote server, so nothing could be submitted.
    NoVoteServers,
    /// The document names no PIR endpoint, so delegation could not prove.
    ///
    /// Stage publishes a single PIR endpoint, so there is no spare: an empty
    /// list is the difference between one endpoint and none.
    NoPirEndpoints,
}

impl std::fmt::Display for StageConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(error) => write!(f, "stage dynamic config is malformed: {error}"),
            Self::NoVoteServers => f.write_str("stage dynamic config names no vote servers"),
            Self::NoPirEndpoints => f.write_str("stage dynamic config names no PIR endpoints"),
        }
    }
}

impl std::error::Error for StageConfigError {}

impl StageDeployment {
    /// Parses and validates the published stage dynamic config.
    ///
    /// Emptiness is rejected here rather than at first use: an empty endpoint
    /// list would otherwise surface as a submission failure deep inside a run,
    /// which reads exactly like the recovery faults this suite hunts for.
    pub fn from_json(bytes: &[u8]) -> Result<Self, StageConfigError> {
        let deployment: Self =
            serde_json::from_slice(bytes).map_err(StageConfigError::Malformed)?;
        if deployment.vote_servers.is_empty() {
            return Err(StageConfigError::NoVoteServers);
        }
        if deployment.pir_endpoints.is_empty() {
            return Err(StageConfigError::NoPirEndpoints);
        }
        Ok(deployment)
    }

    /// Vote-server URLs in published order.
    ///
    /// Order is preserved because the submission lifecycle cycles endpoints by
    /// reservation ordinal; reordering them would change which endpoint a retry
    /// lands on.
    pub fn vote_server_urls(&self) -> Vec<String> {
        self.vote_servers
            .iter()
            .map(|endpoint| endpoint.url.clone())
            .collect()
    }

    /// PIR endpoint URLs in published order.
    pub fn pir_urls(&self) -> Vec<String> {
        self.pir_endpoints
            .iter()
            .map(|endpoint| endpoint.url.clone())
            .collect()
    }
}
