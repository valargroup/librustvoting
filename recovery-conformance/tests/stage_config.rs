//! Reading the published staging configuration.
//!
//! The fixture is a verbatim excerpt of the real stage document, so a change to
//! its shape shows up here rather than as an endpoint failure mid-run.

use recovery_conformance::stage_config::{StageConfigError, StageDeployment};

/// The published stage config's own shape, round entries elided.
const STAGE_EXCERPT: &str = r#"{
  "config_version": 1,
  "vote_servers": [
    { "url": "https://stage.vote-chain-primary.valargroup.org", "label": "valarg-genesis" },
    { "url": "https://stage.vote-chain-secondary.valargroup.org", "label": "valarg-secondary" }
  ],
  "pir_endpoints": [
    { "url": "https://stage.pir.valargroup.org", "label": "PIR primary" }
  ],
  "pir_layout": { "pir_depth": 19, "tier0_layers": 12, "tier1_layers": 7, "poly_len": 4096 },
  "supported_versions": {
    "pir": ["v0"],
    "vote_protocol": "v0",
    "tally": "v0",
    "vote_server": "v1"
  },
  "rounds": {
    "01f6b702558a8b84493cd01a162ccee513f672c5564e119750a9ec231eebae16": {
      "auth_version": 1,
      "ea_pk": "0fwUIpA4Oq88ImoqgAdaVW+tX2L3f8zU4AviTxNRO4g=",
      "signatures": [
        { "key_id": "valar-test-keplr-derived", "alg": "ed25519", "sig": "WMOl" }
      ]
    }
  }
}"#;

#[test]
fn the_published_stage_shape_parses() {
    let deployment = StageDeployment::from_json(STAGE_EXCERPT.as_bytes()).unwrap();
    assert_eq!(deployment.config_version, 1);
    assert_eq!(deployment.supported_versions.vote_protocol, "v0");
    assert_eq!(deployment.supported_versions.vote_server, "v1");
    // The v2 round-auth signature binds the layout, so a round entry signed
    // against the wrong one fails verification in the wallet.
    assert_eq!(deployment.pir_layout.pir_depth, 19);
    assert_eq!(deployment.pir_layout.poly_len, 4096);
}

#[test]
fn endpoint_order_is_preserved() {
    // The submission lifecycle cycles endpoints by reservation ordinal, so
    // reordering changes which endpoint a retry lands on.
    let deployment = StageDeployment::from_json(STAGE_EXCERPT.as_bytes()).unwrap();
    assert_eq!(
        deployment.vote_server_urls(),
        vec![
            "https://stage.vote-chain-primary.valargroup.org".to_string(),
            "https://stage.vote-chain-secondary.valargroup.org".to_string(),
        ]
    );
    // Stage publishes exactly one PIR endpoint, so delegation has no failover
    // there. Worth knowing when a PIR blip looks like a recovery fault.
    assert_eq!(
        deployment.pir_urls(),
        vec!["https://stage.pir.valargroup.org".to_string()]
    );
}

#[test]
fn round_entries_are_ignored_rather_than_adopted() {
    // Stage publishes both generations (245 v1, 152 v2 as of 2026-09-05), and
    // this crate accepts only v2. Parsing must not depend on either: the suite
    // provisions its own round and signs its own v2 entry.
    assert!(StageDeployment::from_json(STAGE_EXCERPT.as_bytes()).is_ok());
}

#[test]
fn a_deployment_with_no_vote_servers_is_refused_up_front() {
    // Caught at startup, because an empty endpoint list surfaces later as a
    // submission failure that reads exactly like a recovery fault.
    let json = r#"{
      "config_version": 1,
      "vote_servers": [],
      "pir_endpoints": [{ "url": "https://pir.example", "label": "a" }],
      "pir_layout": { "pir_depth": 19, "tier0_layers": 12, "tier1_layers": 7, "poly_len": 4096 },
      "supported_versions": { "pir": ["v0"], "vote_protocol": "v0", "tally": "v0", "vote_server": "v1" }
    }"#;
    assert!(matches!(
        StageDeployment::from_json(json.as_bytes()),
        Err(StageConfigError::NoVoteServers)
    ));
}

#[test]
fn a_deployment_with_no_pir_endpoints_is_refused_up_front() {
    let json = r#"{
      "config_version": 1,
      "vote_servers": [{ "url": "https://vote.example", "label": "a" }],
      "pir_endpoints": [],
      "pir_layout": { "pir_depth": 19, "tier0_layers": 12, "tier1_layers": 7, "poly_len": 4096 },
      "supported_versions": { "pir": ["v0"], "vote_protocol": "v0", "tally": "v0", "vote_server": "v1" }
    }"#;
    assert!(matches!(
        StageDeployment::from_json(json.as_bytes()),
        Err(StageConfigError::NoPirEndpoints)
    ));
}

// --- endpoints built from a deployment -------------------------------------

use recovery_conformance::helper_fleet::{HelperFleetPlan, SYNTHETIC_HELPER_URLS};
use recovery_conformance::round_run::{endpoints_from, endpoints_with_fleet, helper_backend};

#[test]
fn the_real_helper_is_the_primary_vote_server() {
    // Helpers are not PIR. The share endpoint lives on the vote server, and
    // only the primary answers it on staging — the secondary and the PIR host
    // both return 404. Pointing share delivery anywhere else fails as
    // `HelperDeliveryIncomplete`, which reads like a delivery defect rather
    // than a misconfiguration.
    let deployment = StageDeployment::from_json(STAGE_EXCERPT.as_bytes()).unwrap();
    assert_eq!(
        helper_backend(&deployment),
        "https://stage.vote-chain-primary.valargroup.org"
    );
    assert_eq!(
        endpoints_from(&deployment).helper_urls,
        vec!["https://stage.vote-chain-primary.valargroup.org".to_string()],
        "without a fleet the suite drives the one real helper"
    );
}

#[test]
fn a_fleet_plan_replaces_the_configured_helpers_entirely() {
    // The wiring that makes a fleet visible to the SDK at all. If this fell
    // back to the single real endpoint, the fleet matrix would drive one helper
    // with a target count of one, every placement assertion would hold
    // trivially, and the whole axis would report green having tested the
    // degenerate case it exists to escape.
    let deployment = StageDeployment::from_json(STAGE_EXCERPT.as_bytes()).unwrap();
    let backend = helper_backend(&deployment);
    let endpoints = endpoints_with_fleet(&deployment, &HelperFleetPlan::all_answering(&backend, 10));

    assert_eq!(endpoints.helper_urls.len(), 10);
    assert_eq!(endpoints.helper_urls, SYNTHETIC_HELPER_URLS.to_vec());
    assert!(
        !endpoints.helper_urls.contains(&backend),
        "the real endpoint must not also appear as a helper; it would be an \
         eleventh identity sharing a backend with the other ten"
    );
}

#[test]
fn a_fleet_plan_leaves_every_other_endpoint_alone() {
    // Only helper delivery is redirected. Chain, PIR, and tree traffic must
    // still reach the real staging deployment, or a fleet scenario would stop
    // being a real round.
    let deployment = StageDeployment::from_json(STAGE_EXCERPT.as_bytes()).unwrap();
    let plain = endpoints_from(&deployment);
    let fleeted = endpoints_with_fleet(
        &deployment,
        &HelperFleetPlan::all_answering(helper_backend(&deployment), 10),
    );
    assert_eq!(fleeted.vote_servers, plain.vote_servers);
    assert_eq!(fleeted.pir_urls, plain.pir_urls);
    assert_eq!(fleeted.chain_rpc, plain.chain_rpc);
    assert_eq!(fleeted.lightwalletd, plain.lightwalletd);
}

#[test]
fn an_empty_fleet_plan_keeps_the_real_helper() {
    // What every crash and stall exercise relies on: an empty plan changes
    // nothing about where the round sends its shares.
    let deployment = StageDeployment::from_json(STAGE_EXCERPT.as_bytes()).unwrap();
    assert_eq!(
        endpoints_with_fleet(&deployment, &HelperFleetPlan::none()).helper_urls,
        endpoints_from(&deployment).helper_urls
    );
}
