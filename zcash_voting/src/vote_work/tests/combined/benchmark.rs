//! Repeatable real-ZKP2 bundle pipelines over deterministic delayed peers.

use super::*;
use futures_util::{stream, StreamExt};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "release benchmark; make bench-bundle-pipelines"]
async fn bundle_pipeline_benchmark() {
    let Ok(concurrency) = std::env::var("BUNDLE_BENCH_CONCURRENCY") else {
        return;
    };
    let concurrency: usize = concurrency.parse().unwrap();
    let bundles: usize = std::env::var("BUNDLE_BENCH_COUNT")
        .unwrap_or_else(|_| "6".into())
        .parse()
        .unwrap();
    let delay: u64 = std::env::var("BUNDLE_BENCH_DELAY_MS")
        .unwrap_or_else(|_| "250".into())
        .parse()
        .unwrap();
    assert!(concurrency > 0 && bundles > 0);
    let mut policy = ProvingPolicy::default();
    if let Ok(workers) = std::env::var("BUNDLE_BENCH_WORKERS") {
        policy.cpu_worker_count = workers.parse().unwrap();
    }
    if let Ok(jobs) = std::env::var("BUNDLE_BENCH_HEAVY_JOBS") {
        policy.max_active_heavy_jobs = jobs.parse().unwrap();
    }
    configure_proving_runtime(policy).unwrap();
    if std::env::var("BUNDLE_BENCH_CACHE").as_deref() == Ok("warm") {
        warm_zkp2_proving_cache().unwrap();
    }
    let started = Instant::now();
    let reports = stream::iter(0..bundles)
        .map(|_| async {
            let (db, driver) = database();
            let peers = Arc::new(Peers {
                db: db.clone(),
                posts: Default::default(),
                deliveries: Default::default(),
                rejections: Default::default(),
                delay: Duration::from_millis(delay),
            });
            let executor = executor(db, peers.clone(), true);
            executor
                .set_ballot_intents(&[BallotIntent {
                    proposal_id: 2,
                    decision: session::Decision::Choice(1),
                }])
                .unwrap();
            let host = context(driver);
            let control = ChainSubmissionControl::new(1);
            let report = RoundDriver::new(&executor)
                .run_with_report(
                    &RoundHostSourceBridge::new(|| host.clone()),
                    &control,
                    &NoopRoundDriveReporter {},
                    Some(ObservabilityOptions::default()),
                )
                .await;
            assert!(
                report.result.failures.is_empty(),
                "{:?}",
                report.result.failures
            );
            assert_completed(&executor, &peers, 1);
            report.observability.unwrap()
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
    let queue_wait_us: u64 = reports
        .iter()
        .flat_map(|report| &report.records)
        .filter(|record| record.stage.as_ref() == "proving::admission_wait")
        .map(|record| record.elapsed_us)
        .sum();
    println!(
        "BUNDLE_BENCH {}",
        serde_json::json!({
            "concurrency": concurrency, "bundles": bundles, "proposals_per_bundle": 2,
            "wall_seconds": started.elapsed().as_secs_f64(), "worker_count": policy.cpu_worker_count.get(),
            "heavy_job_limit": policy.max_active_heavy_jobs.get(), "admission_wait_us": queue_wait_us,
            "transport_delay_ms": delay, "cache": std::env::var("BUNDLE_BENCH_CACHE").unwrap_or_else(|_| "cold".into()),
        })
    );
}
