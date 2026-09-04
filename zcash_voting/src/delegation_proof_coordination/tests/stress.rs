//! Adversarial concurrency coverage for delegation proof coordination.
//!
//! Every test here runs behind a wall-clock deadline so a regression surfaces
//! as a failed assertion naming the hazard instead of a hung test binary.
//! The properties exercised are:
//!
//! - mutual exclusion per identity under heavy contention and lock churn;
//! - liveness: every caller eventually returns, including after a producer
//!   panics or fails while waiters are queued;
//! - thread-local state is restored after panics and after `Busy` rejections;
//! - a progress callback that synchronously hands the same identity to another
//!   thread must not deadlock the process.

use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Barrier, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use crate::VotingError;

use super::super::{coordinate, DelegationProofIdentity};

const DEADLINE: Duration = Duration::from_secs(30);

fn identity(wallet_id: &str, bundle_index: u32) -> DelegationProofIdentity {
    DelegationProofIdentity::new(0, wallet_id.to_string(), "stress-round", bundle_index)
}

/// Runs `work` on a fresh thread and fails the test with `hazard` if it does
/// not finish inside [`DEADLINE`]. A timed-out thread is left blocked on
/// purpose; identities in this file are unique per test so it cannot leak into
/// another test's lock.
fn finishes_within_deadline<T: Send + 'static>(
    hazard: &str,
    work: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (done, finished) = mpsc::channel();
    thread::spawn(move || {
        let _ = done.send(work());
    });
    finished
        .recv_timeout(DEADLINE)
        .unwrap_or_else(|_| panic!("deadlock suspected: {hazard}"))
}

fn spin_until(flag: &AtomicBool) {
    let started = Instant::now();
    while !flag.load(Ordering::SeqCst) {
        assert!(started.elapsed() < DEADLINE, "flag never set");
        thread::yield_now();
    }
}

/// Tracks how many operations are concurrently admitted per identity and
/// records the largest overlap ever observed.
#[derive(Default)]
struct OverlapMeter {
    active: Vec<AtomicUsize>,
    max_active: Vec<AtomicUsize>,
    completed: AtomicUsize,
}

impl OverlapMeter {
    fn new(identity_count: usize) -> Self {
        Self {
            active: (0..identity_count).map(|_| AtomicUsize::new(0)).collect(),
            max_active: (0..identity_count).map(|_| AtomicUsize::new(0)).collect(),
            completed: AtomicUsize::new(0),
        }
    }

    fn enter(&self, slot: usize) {
        let now_active = self.active[slot].fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active[slot].fetch_max(now_active, Ordering::SeqCst);
    }

    fn leave(&self, slot: usize) {
        self.active[slot].fetch_sub(1, Ordering::SeqCst);
        self.completed.fetch_add(1, Ordering::SeqCst);
    }

    fn assert_exclusive(&self, expected_completions: usize) {
        for (slot, max_active) in self.max_active.iter().enumerate() {
            assert_eq!(
                max_active.load(Ordering::SeqCst),
                1,
                "identity slot {slot} admitted overlapping operations"
            );
        }
        assert_eq!(self.completed.load(Ordering::SeqCst), expected_completions);
    }
}

/// Deterministic per-thread pseudo-random stream; no external RNG needed.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

#[test]
fn hammered_identities_never_overlap_and_every_caller_returns() {
    const THREADS: usize = 8;
    const ITERATIONS: usize = 150;
    const IDENTITIES: usize = 3;

    let meter = Arc::new(OverlapMeter::new(IDENTITIES));
    let start = Arc::new(Barrier::new(THREADS));

    let workers: Vec<_> = (0..THREADS)
        .map(|thread_index| {
            let meter = Arc::clone(&meter);
            let start = Arc::clone(&start);
            thread::spawn(move || {
                let mut rng = Lcg(thread_index as u64 + 1);
                start.wait();
                let mut failures = 0usize;
                for _ in 0..ITERATIONS {
                    let slot = (rng.next() % IDENTITIES as u64) as usize;
                    let should_fail = rng.next() % 5 == 0;
                    let spin = rng.next() % 50;
                    let outcome = coordinate(
                        identity("hammer-wallet", slot as u32),
                        || {},
                        |_| {
                            meter.enter(slot);
                            for _ in 0..spin {
                                std::hint::spin_loop();
                            }
                            if rng.next() % 3 == 0 {
                                thread::yield_now();
                            }
                            meter.leave(slot);
                            if should_fail {
                                Err(VotingError::ProofFailed {
                                    message: "synthetic failure".to_string(),
                                })
                            } else {
                                Ok(())
                            }
                        },
                    );
                    match outcome {
                        Ok(()) => assert!(!should_fail),
                        Err(VotingError::ProofFailed { .. }) => {
                            assert!(should_fail);
                            failures += 1;
                        }
                        Err(other) => panic!("unexpected coordination error: {other}"),
                    }
                }
                failures
            })
        })
        .collect();

    let total_failures = finishes_within_deadline("hammer workers did not all return", move || {
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .sum::<usize>()
    });

    meter.assert_exclusive(THREADS * ITERATIONS);
    assert!(total_failures > 0, "failure path was never exercised");
}

#[test]
fn two_thread_ping_pong_survives_lock_teardown_churn() {
    // Zero-length operations maximize the window in which one thread tears
    // down the registry entry while the other tries to re-enter it.
    const ITERATIONS: usize = 4_000;

    let meter = Arc::new(OverlapMeter::new(1));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let meter = Arc::clone(&meter);
            thread::spawn(move || {
                for _ in 0..ITERATIONS {
                    coordinate(
                        identity("churn-wallet", 0),
                        || {},
                        |_| {
                            meter.enter(0);
                            meter.leave(0);
                            Ok(())
                        },
                    )
                    .unwrap();
                }
            })
        })
        .collect();

    finishes_within_deadline("ping-pong workers did not return", move || {
        for worker in workers {
            worker.join().unwrap();
        }
    });
    meter.assert_exclusive(2 * ITERATIONS);
}

#[test]
fn panicking_producer_releases_waiters_and_its_own_thread() {
    let follower_waiting = Arc::new(AtomicBool::new(false));
    let leader_admitted = Arc::new(AtomicBool::new(false));

    let leader = {
        let follower_waiting = Arc::clone(&follower_waiting);
        let leader_admitted = Arc::clone(&leader_admitted);
        thread::spawn(move || {
            let panicked = catch_unwind(AssertUnwindSafe(|| {
                coordinate(
                    identity("panic-wallet", 0),
                    || {},
                    |_| -> Result<(), VotingError> {
                        leader_admitted.store(true, Ordering::SeqCst);
                        spin_until(&follower_waiting);
                        panic!("prover crashed while a waiter was queued");
                    },
                )
            }))
            .is_err();
            assert!(panicked);

            // The unwinding thread must have cleared its own reentrancy marker,
            // otherwise every later proof on this thread is spuriously Busy.
            coordinate(identity("panic-wallet", 0), || {}, |_| Ok("after panic"))
        })
    };
    spin_until(&leader_admitted);

    let follower = {
        let follower_waiting = Arc::clone(&follower_waiting);
        thread::spawn(move || {
            coordinate(
                identity("panic-wallet", 0),
                || follower_waiting.store(true, Ordering::SeqCst),
                |_| Ok("follower ran after poison"),
            )
        })
    };

    let (leader_result, follower_result) =
        finishes_within_deadline("poisoned proof lock stranded a waiter", move || {
            (leader.join().unwrap(), follower.join().unwrap())
        });
    assert_eq!(follower_result.unwrap(), "follower ran after poison");
    assert_eq!(leader_result.unwrap(), "after panic");
}

#[test]
fn panicking_wait_callback_restores_the_calling_thread() {
    let leader_admitted = Arc::new(AtomicBool::new(false));
    let release_leader = Arc::new(AtomicBool::new(false));
    let leader = {
        let leader_admitted = Arc::clone(&leader_admitted);
        let release_leader = Arc::clone(&release_leader);
        thread::spawn(move || {
            coordinate(
                identity("wait-panic-wallet", 0),
                || {},
                |_| {
                    leader_admitted.store(true, Ordering::SeqCst);
                    spin_until(&release_leader);
                    Ok(())
                },
            )
        })
    };
    spin_until(&leader_admitted);

    let follower = {
        let release_leader = Arc::clone(&release_leader);
        thread::spawn(move || {
            let panicked = catch_unwind(AssertUnwindSafe(|| {
                coordinate(
                    identity("wait-panic-wallet", 0),
                    || panic!("host wait notification failed"),
                    |_| Ok(()),
                )
            }))
            .is_err();
            assert!(panicked);
            release_leader.store(true, Ordering::SeqCst);

            // Same thread, same identity: must wait and succeed, not Busy.
            coordinate(identity("wait-panic-wallet", 0), || {}, |_| Ok("recovered"))
        })
    };

    let (leader_result, follower_result) =
        finishes_within_deadline("panicking wait callback wedged the thread", move || {
            (leader.join().unwrap(), follower.join().unwrap())
        });
    leader_result.unwrap();
    assert_eq!(follower_result.unwrap(), "recovered");
}

#[test]
fn busy_rejection_does_not_disturb_outer_or_later_operations() {
    let nested_outcomes = Arc::new(Mutex::new(Vec::new()));
    let outcomes = Arc::clone(&nested_outcomes);

    let outer = coordinate(
        identity("busy-wallet", 0),
        || {},
        |_| {
            for bundle_index in [0, 1] {
                let nested = coordinate(identity("busy-wallet", bundle_index), || {}, |_| Ok(()));
                outcomes
                    .lock()
                    .unwrap()
                    .push(matches!(nested, Err(VotingError::Busy { .. })));
            }
            Ok("outer completed")
        },
    );
    assert_eq!(outer.unwrap(), "outer completed");
    assert_eq!(*nested_outcomes.lock().unwrap(), vec![true, true]);

    // A rejected nested call must not have cleared the outer marker early or
    // left a stale marker behind: both identities are usable again.
    assert_eq!(
        coordinate(identity("busy-wallet", 0), || {}, |_| Ok(0)).unwrap(),
        0
    );
    assert_eq!(
        coordinate(identity("busy-wallet", 1), || {}, |_| Ok(1)).unwrap(),
        1
    );

    // And the rejected identity's lock was never taken: another thread enters
    // it immediately without waiting.
    let waited = Arc::new(AtomicBool::new(false));
    let waited_flag = Arc::clone(&waited);
    finishes_within_deadline("stale lock left behind by Busy rejection", move || {
        coordinate(
            identity("busy-wallet", 1),
            || waited_flag.store(true, Ordering::SeqCst),
            |_| Ok(()),
        )
        .unwrap();
    });
    assert!(!waited.load(Ordering::SeqCst));
}

#[test]
fn queued_waiters_after_failed_producer_generate_exactly_once() {
    const WAITERS: usize = 6;

    let leader_admitted = Arc::new(AtomicBool::new(false));
    let waiting_count = Arc::new(AtomicUsize::new(0));
    let generated = Arc::new(AtomicBool::new(false));
    let generation_count = Arc::new(AtomicUsize::new(0));
    let meter = Arc::new(OverlapMeter::new(1));

    let leader = {
        let leader_admitted = Arc::clone(&leader_admitted);
        let waiting_count = Arc::clone(&waiting_count);
        let meter = Arc::clone(&meter);
        thread::spawn(move || {
            coordinate(
                identity("queue-wallet", 0),
                || {},
                |_| {
                    meter.enter(0);
                    leader_admitted.store(true, Ordering::SeqCst);
                    let started = Instant::now();
                    while waiting_count.load(Ordering::SeqCst) < WAITERS {
                        assert!(started.elapsed() < DEADLINE, "waiters never queued");
                        thread::yield_now();
                    }
                    meter.leave(0);
                    Err::<(), _>(VotingError::ProofFailed {
                        message: "leader failed".to_string(),
                    })
                },
            )
        })
    };
    spin_until(&leader_admitted);

    let waiters: Vec<_> = (0..WAITERS)
        .map(|_| {
            let waiting_count = Arc::clone(&waiting_count);
            let generated = Arc::clone(&generated);
            let generation_count = Arc::clone(&generation_count);
            let meter = Arc::clone(&meter);
            thread::spawn(move || {
                coordinate(
                    identity("queue-wallet", 0),
                    || {
                        waiting_count.fetch_add(1, Ordering::SeqCst);
                    },
                    |_| {
                        meter.enter(0);
                        let reused = if generated.load(Ordering::SeqCst) {
                            true
                        } else {
                            generation_count.fetch_add(1, Ordering::SeqCst);
                            generated.store(true, Ordering::SeqCst);
                            false
                        };
                        meter.leave(0);
                        Ok(reused)
                    },
                )
            })
        })
        .collect();

    let (leader_result, reuse_flags) =
        finishes_within_deadline("queued waiters did not drain", move || {
            let leader_result = leader.join().unwrap();
            let flags: Vec<bool> = waiters
                .into_iter()
                .map(|waiter| waiter.join().unwrap().unwrap())
                .collect();
            (leader_result, flags)
        });

    assert!(matches!(
        leader_result,
        Err(VotingError::ProofFailed { .. })
    ));
    assert_eq!(generation_count.load(Ordering::SeqCst), 1);
    assert_eq!(reuse_flags.iter().filter(|reused| !**reused).count(), 1);
    meter.assert_exclusive(WAITERS + 1);
}
