//! The run directory as the interface it is.
//!
//! A finished run is read back by `analyze`, by a later comparison, and by a
//! person. Everything it holds therefore has to survive a round trip through
//! disk without the workload and the numbers drifting apart.

use std::path::PathBuf;

use recovery_conformance::helper_fleet::HelperFleetPlan;
use recovery_conformance::run_config::Endpoints;
use stage_bench::ballot::Ballot;
use stage_bench::metrics::{render, Metrics};
use stage_bench::run_config::{BenchOutcome, BenchRunConfig, TrackingSummary};
use stage_bench::Manifest;

fn scratch(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "stage-bench-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("a scratch directory");
    directory
}

fn config(run_dir: &std::path::Path) -> BenchRunConfig {
    BenchRunConfig {
        sidecar: run_dir.join("sidecar.db"),
        wallet_db: PathBuf::from("/tmp/voter.db"),
        warm_pir_from: Some(PathBuf::from("/tmp/pir-warm.db")),
        round_id: "0123456789abcdef".to_string(),
        account_uuid: "8b29d4e6-7940-4570-b2c2-3c7a25ba6922".to_string(),
        endpoints: Endpoints {
            chain_rpc: "https://stage.vote-rpc-primary.valargroup.org".to_string(),
            vote_servers: vec!["https://stage.vote-chain-primary.valargroup.org".to_string()],
            pir_urls: vec!["https://stage.pir.valargroup.org".to_string()],
            helper_urls: vec!["https://stage.vote-chain-primary.valargroup.org".to_string()],
            lightwalletd: "https://testnet.zec.rocks:443".to_string(),
        },
        ballot: Ballot::synthetic(37, &[2, 3, 4]).expect("a benchmark ballot"),
        fleet: HelperFleetPlan::none(),
        vote_end_time_seconds: 1_800_000_000,
        bundle_concurrency: 1,
        max_dispatches: 8_192,
        max_records: 262_144,
        run_dir: run_dir.to_path_buf(),
    }
}

fn outcome() -> BenchOutcome {
    BenchOutcome {
        quiescence: "BackgroundShareWorkOnly { shares: 3 }".to_string(),
        quiescence_kind: "BackgroundShareWorkOnly".to_string(),
        failures: Vec::new(),
        notes: 11,
        bundles: 3,
        proposals: 37,
        completed_proposals: 37,
        tracking: vec![TrackingSummary {
            quiescence: "NothingToTrack".to_string(),
            passes: 2,
            confirmed: 1_776,
            ..TrackingSummary::default()
        }],
        round_drive_seconds: 402.5,
        tracking_seconds: 51.0,
    }
}

#[test]
fn a_run_configuration_survives_the_file_it_is_passed_through() {
    let run_dir = scratch("config");
    let original = config(&run_dir);

    let path = BenchRunConfig::path_in(&run_dir);
    original.write(&path).expect("writing the configuration");
    let read = BenchRunConfig::read(&path).expect("reading it back");

    assert_eq!(read.round_id, original.round_id);
    assert_eq!(read.ballot, original.ballot);
    assert_eq!(read.ballot.len(), 37);
    assert_eq!(read.endpoints.vote_servers, original.endpoints.vote_servers);
    assert_eq!(read.vote_end_time_seconds, original.vote_end_time_seconds);
    assert_eq!(read.max_records, original.max_records);

    let _ = std::fs::remove_dir_all(&run_dir);
}

/// The configuration file carries nothing a `ps` listing must not show.
///
/// Credentials reach the worker through the inherited environment. If a seed,
/// a mnemonic, or a signing key ever appeared here it would be written to disk
/// on every run and kept in the run directory indefinitely.
#[test]
fn a_run_configuration_holds_no_secret() {
    // Not named for what it checks: the directory path is inside the file, so a
    // scratch directory called "secrets" would fail this test on its own name.
    let run_dir = scratch("redaction");
    let path = BenchRunConfig::path_in(&run_dir);
    config(&run_dir).write(&path).expect("writing it");

    let raw = std::fs::read_to_string(&path).expect("reading it");
    for forbidden in ["mnemonic", "seed", "secret", "hotkey", "VOTE_SDK_VOTER"] {
        assert!(
            !raw.to_lowercase().contains(&forbidden.to_lowercase()),
            "the run configuration mentions {forbidden}"
        );
    }

    let _ = std::fs::remove_dir_all(&run_dir);
}

#[test]
fn an_outcome_names_the_states_a_finished_round_may_end_in() {
    let mut outcome = outcome();
    assert!(outcome.is_complete());

    outcome.quiescence_kind = "NoWorkLeft".to_string();
    assert!(outcome.is_complete());

    // Every other quiescence needs the host to act or names a fault, and a
    // benchmark over one of those timed a round that did not finish.
    outcome.quiescence_kind = "PassBudgetExhausted".to_string();
    assert!(!outcome.is_complete());

    outcome.quiescence_kind = "NoWorkLeft".to_string();
    outcome
        .failures
        .push(stage_bench::run_config::FailureRecord {
            step: None,
            bundle_index: Some(0),
            kind: "HelperDeliveryIncomplete".to_string(),
            message: "a helper never accepted".to_string(),
        });
    assert!(!outcome.is_complete());
}

#[test]
fn a_manifest_records_the_workload_beside_the_numbers() {
    let run_dir = scratch("manifest");
    let config = config(&run_dir);
    let manifest = Manifest::build(&config, &outcome(), 1_700_000_000, 21_600);
    manifest.write(&run_dir).expect("writing the manifest");

    let read = Manifest::read(&run_dir).expect("reading it back");
    assert_eq!(read.proposals, 37);
    assert_eq!(read.bundles, 3);
    assert_eq!(read.ballot.len(), 37);
    assert_eq!(read.configured_helpers, 1);
    assert!(!read.synthetic_fleet);
    assert_eq!(read.vote_window_seconds, 21_600);
    assert!(read.warm_pir);
    assert_eq!(read.quiescence_kind, "BackgroundShareWorkOnly");
    assert_eq!(read.completed_proposals, 37);
    // A debug build's proving times measure the compiler, so which profile
    // produced a number is part of the number.
    assert!(read.profile == "debug" || read.profile == "release");

    let _ = std::fs::remove_dir_all(&run_dir);
}

/// A finished directory renders without needing anything else.
///
/// This is what `analyze` does. The table is built from the manifest and the
/// snapshots alone, so a run archived weeks ago still reports.
#[test]
fn a_finished_run_directory_renders_its_report() {
    let run_dir = scratch("render");
    let config = config(&run_dir);
    let manifest = Manifest::build(&config, &outcome(), 1_700_000_000, 21_600);
    manifest.write(&run_dir).expect("writing the manifest");

    let snapshot = serde_json::json!({
        "operation": "round::run",
        "started_at_unix_us": 1_700_000_000_000_000u64,
        "round_id": "0123456789abcdef",
        "elapsed_us": 402_500_000u64,
        "outcome": "succeeded",
        "records": [
            {
                "id": 1, "parent_id": null, "stage": "helper::active_delivery",
                "attribution": { "bundle_index": 0, "proposal_id": 1, "share_index": 0 },
                "started_after_us": 0, "elapsed_us": 250_000, "outcome": "succeeded",
                "error_kind": null, "http_status": null, "endpoint_index": 0, "attempt": null
            },
            {
                "id": 2, "parent_id": 1, "stage": "helper.http.post_json",
                "attribution": { "bundle_index": 0, "proposal_id": 1, "share_index": 0 },
                "started_after_us": 10_000, "elapsed_us": 200_000, "outcome": "succeeded",
                "error_kind": null, "http_status": 200, "endpoint_index": 0, "attempt": 1
            }
        ],
        "summaries": [],
        "records_dropped": 0,
        "summary_updates_dropped": 0,
        "active_stages_dropped": 0
    });
    std::fs::write(
        run_dir.join("round.observability.json"),
        serde_json::to_vec(&snapshot).expect("encoding the snapshot"),
    )
    .expect("writing the snapshot");

    let snapshots = stage_bench::read_snapshots(&run_dir).expect("reading the snapshots");
    assert_eq!(snapshots.len(), 1);

    let metrics = Metrics::derive(&snapshots, &[]);
    assert!(metrics.complete);
    assert_eq!(metrics.delivery.active_shares.peak, 1);
    assert_eq!(metrics.delivery.initial_http.samples, 1);
    assert_eq!(metrics.delivery.http_status.get(&200), Some(&1));

    let table = render(&manifest, &metrics);
    assert!(table.contains("0123456789abcdef"));
    assert!(table.contains("helper::active_delivery"));
    assert!(table.contains("37 proposals x 3 bundles"));
    assert!(!table.contains("INCOMPLETE CAPTURE"));

    let _ = std::fs::remove_dir_all(&run_dir);
}

/// A capped capture says so where the numbers are read, not only in the JSON.
#[test]
fn an_incomplete_capture_is_announced_in_the_table() {
    let run_dir = scratch("incomplete");
    let config = config(&run_dir);
    let manifest = Manifest::build(&config, &outcome(), 1_700_000_000, 21_600);

    let snapshot = serde_json::json!({
        "operation": "round::run",
        "started_at_unix_us": 0u64,
        "round_id": null,
        "elapsed_us": 1u64,
        "outcome": "succeeded",
        "records": [],
        "summaries": [],
        "records_dropped": 12,
        "summary_updates_dropped": 0,
        "active_stages_dropped": 3
    });
    let captured = stage_bench::CapturedSnapshot {
        source: "round.observability.json".to_string(),
        snapshot: serde_json::from_value(snapshot).expect("a decodable snapshot"),
    };
    let metrics = Metrics::derive(&[captured], &[]);

    assert!(!metrics.complete);
    let table = render(&manifest, &metrics);
    assert!(table.contains("INCOMPLETE CAPTURE"));
    assert!(table.contains("12 records"));
    assert!(table.contains("3 stage starts"));

    let _ = std::fs::remove_dir_all(&run_dir);
}

/// A run whose worker died before writing an outcome still has a directory.
#[test]
fn snapshots_absent_from_a_directory_are_not_an_error() {
    let run_dir = scratch("empty");
    assert!(stage_bench::read_snapshots(&run_dir)
        .expect("an empty directory reads")
        .is_empty());
    let _ = std::fs::remove_dir_all(&run_dir);
}
