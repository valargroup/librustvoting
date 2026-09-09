//! Resource and lifecycle conformance without circuit-shaped test substitutes.

use super::*;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Barrier,
    },
    time::Duration,
};

fn runtime_with(workers: usize, jobs: usize) -> Runtime {
    Runtime::new(ProvingPolicy {
        cpu_worker_count: NonZeroUsize::new(workers).unwrap(),
        max_active_heavy_jobs: NonZeroUsize::new(jobs).unwrap(),
    })
    .unwrap()
}

#[test]
fn independent_callers_share_the_heavy_job_limit_and_worker_pool() {
    let runtime = runtime_with(3, 2);
    let active = AtomicUsize::new(0);
    let maximum = AtomicUsize::new(0);
    let start = Barrier::new(7);
    std::thread::scope(|scope| {
        for _ in 0..6 {
            scope.spawn(|| {
                start.wait();
                runtime
                    .execute(&crate::ObservationScope::disabled(), || {
                        assert!(runtime.pool.current_thread_index().is_some());
                        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                        maximum.fetch_max(count, Ordering::SeqCst);
                        rayon::join(
                            || assert!(runtime.pool.current_thread_index().is_some()),
                            || assert!(runtime.pool.current_thread_index().is_some()),
                        );
                        std::thread::sleep(Duration::from_millis(25));
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap();
            });
        }
        start.wait();
    });
    assert_eq!(maximum.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.pool.current_num_threads(), 3);
}

#[test]
fn reverse_completion_retains_canonical_action_order_and_local_limit() {
    let runtime = runtime_with(3, 3);
    let active = AtomicUsize::new(0);
    let maximum = AtomicUsize::new(0);
    let result = runtime
        .execute_many(6, 2, &crate::ObservationScope::disabled(), |index| {
            let count = active.fetch_add(1, Ordering::SeqCst) + 1;
            maximum.fetch_max(count, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(if index % 2 == 0 { 20 } else { 1 }));
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(index)
        })
        .unwrap();
    assert_eq!(result, (0..6).collect::<Vec<_>>());
    assert_eq!(maximum.load(Ordering::SeqCst), 2);
}

#[test]
fn panic_and_error_release_capacity_and_batch_drains() {
    let runtime = runtime_with(1, 1);
    let observations = crate::ObservationScope::disabled();
    assert!(runtime
        .execute::<()>(&observations, || panic!("test proof panic"))
        .is_err());
    assert!(runtime
        .execute_many::<()>(4, 3, &observations, |_| Err(internal("test proof error")))
        .is_err());
    runtime.execute(&observations, || Ok(())).unwrap();
}

#[test]
fn cancellation_removes_a_queued_job_without_running_it() {
    let runtime = runtime_with(1, 1);
    let control = crate::ChainSubmissionControl::new(4);
    let operation = Operation::controlled("cancelled-bundle".into(), control.clone(), 4);
    let blocker = runtime
        .admission
        .acquire(&Operation::current(), &crate::ObservationScope::disabled())
        .unwrap();
    let entered = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        let job = scope.spawn(|| {
            operation.enter(|| {
                runtime.execute(&crate::ObservationScope::disabled(), || {
                    entered.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            })
        });
        control.cancel();
        assert!(job.join().unwrap().is_err());
        drop(blocker);
    });
    assert_eq!(entered.load(Ordering::SeqCst), 0);
    runtime
        .execute(&crate::ObservationScope::disabled(), || Ok(()))
        .unwrap();
}

#[test]
fn dropping_an_owner_interrupts_queued_work_and_epoch_changes_are_observed() {
    let control = crate::ChainSubmissionControl::new(7);
    let operation = Operation::controlled("bundle".into(), control.clone(), 7);
    assert!(operation.check().is_ok());
    control.set_operation_epoch(8);
    assert!(operation.check().is_err());
    let operation = Operation::current();
    drop(operation.owner());
    assert!(operation.check().is_err());
}

#[test]
fn configuration_is_immutable_in_a_fresh_process() {
    const CHILD: &str = "VOTING_RUNTIME_CONFIGURATION_TEST";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "proving_runtime::tests::configuration_is_immutable_in_a_fresh_process",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let policy = ProvingPolicy {
        cpu_worker_count: NonZeroUsize::MIN,
        max_active_heavy_jobs: NonZeroUsize::MIN,
    };
    configure_proving_runtime(policy).unwrap();
    configure_proving_runtime(policy).unwrap();
    assert_eq!(
        configure_proving_runtime(ProvingPolicy {
            cpu_worker_count: NonZeroUsize::new(2).unwrap(),
            ..policy
        }),
        Err(ProvingConfigurationError::AlreadyConfigured)
    );
}

#[test]
#[ignore = "real cold key generation; run with make proofs"]
fn one_worker_cold_caches_do_not_deadlock() {
    configure_proving_runtime(ProvingPolicy {
        cpu_worker_count: NonZeroUsize::MIN,
        max_active_heavy_jobs: NonZeroUsize::MIN,
    })
    .unwrap();
    let observations = crate::ObservationScope::disabled();
    ensure_cache(CacheKind::Delegation, &observations).unwrap();
    ensure_cache(CacheKind::Vote, &observations).unwrap();
    execute(&observations, || Ok(())).unwrap();
}

#[tokio::test]
async fn dropped_future_retains_running_proof_exclusion_and_suppresses_later_effects() {
    let runtime = Arc::new(runtime_with(1, 1));
    let exclusion = Arc::new(tokio::sync::Mutex::new(()));
    let held = exclusion.clone().lock_owned().await;
    let effects = Arc::new(AtomicUsize::new(0));
    let (started, entered) = std::sync::mpsc::channel();
    let (release, finish) = std::sync::mpsc::channel();
    let recorded = effects.clone();
    let task = tokio::spawn(async move {
        let operation = Operation::current();
        let _owner = operation.owner();
        tokio::task::spawn_blocking(move || {
            operation.enter(|| {
                let _held = held;
                runtime.execute(&crate::ObservationScope::disabled(), move || {
                    started.send(()).unwrap();
                    finish.recv().unwrap();
                    Ok(())
                })?;
                operation.check()?;
                recorded.fetch_add(1, Ordering::SeqCst);
                Ok::<_, crate::VotingError>(())
            })
        })
        .await
        .unwrap()
    });
    tokio::task::spawn_blocking(move || entered.recv_timeout(Duration::from_secs(3)).unwrap())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let still_excluded = exclusion.try_lock().is_err();
    release.send(()).unwrap();
    assert!(
        still_excluded,
        "a running proof retains bundle exclusion after its future is dropped"
    );
    let _released = tokio::time::timeout(Duration::from_secs(3), exclusion.lock())
        .await
        .unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 0);
}

#[test]
fn single_and_maximum_size_batches_complete_under_one_worker() {
    let runtime = runtime_with(1, 1);
    for count in [1, crate::vote::MAX_VOTE_BATCH_ACTIONS] {
        let proofs = runtime
            .execute_many(count, 3, &crate::ObservationScope::disabled(), Ok)
            .unwrap();
        assert_eq!(proofs, (0..count).collect::<Vec<_>>());
    }
}

#[test]
fn orchestration_remains_usable_without_a_tokio_runtime() {
    let (send, receive) = std::sync::mpsc::channel();
    spawn_orchestration(move || send.send(17).unwrap()).unwrap();
    assert_eq!(receive.recv_timeout(Duration::from_secs(3)).unwrap(), 17);
}
