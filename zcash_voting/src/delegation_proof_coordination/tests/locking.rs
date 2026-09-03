use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Barrier, Condvar, Mutex,
    },
    thread,
};

use super::super::{coordinate, DelegationProofIdentity};

fn identity(wallet_id: &str, bundle_index: u32) -> DelegationProofIdentity {
    DelegationProofIdentity::new(wallet_id.to_string(), "round", bundle_index)
}

#[test]
fn identical_proof_work_waits_and_reuses_durable_completion() {
    let proof_ready = Arc::new(AtomicBool::new(false));
    let generation_count = Arc::new(AtomicUsize::new(0));
    let leader_started = Arc::new(Barrier::new(2));
    let release_leader = Arc::new((Mutex::new(false), Condvar::new()));

    let leader = {
        let proof_ready = proof_ready.clone();
        let generation_count = generation_count.clone();
        let leader_started = leader_started.clone();
        let release_leader = release_leader.clone();
        thread::spawn(move || {
            coordinate(
                identity("wallet", 0),
                || {},
                |_| {
                    generation_count.fetch_add(1, Ordering::SeqCst);
                    leader_started.wait();
                    let (released, wake) = &*release_leader;
                    let mut released = released.lock().unwrap();
                    while !*released {
                        released = wake.wait(released).unwrap();
                    }
                    proof_ready.store(true, Ordering::SeqCst);
                    true
                },
            )
        })
    };
    leader_started.wait();

    let waited = Arc::new(AtomicBool::new(false));
    let follower = {
        let proof_ready = proof_ready.clone();
        let generation_count = generation_count.clone();
        let waited = waited.clone();
        thread::spawn(move || {
            coordinate(
                identity("wallet", 0),
                || waited.store(true, Ordering::SeqCst),
                |_| {
                    if proof_ready.load(Ordering::SeqCst) {
                        false
                    } else {
                        generation_count.fetch_add(1, Ordering::SeqCst);
                        true
                    }
                },
            )
        })
    };

    while !waited.load(Ordering::SeqCst) {
        thread::yield_now();
    }
    let (released, wake) = &*release_leader;
    *released.lock().unwrap() = true;
    wake.notify_all();

    assert!(leader.join().unwrap());
    assert!(!follower.join().unwrap());
    assert_eq!(generation_count.load(Ordering::SeqCst), 1);
}

#[test]
fn different_bundles_enter_proof_work_concurrently() {
    let both_entered = Arc::new(Barrier::new(3));
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::new();

    for bundle_index in 0..2 {
        let both_entered = both_entered.clone();
        let active = active.clone();
        let max_active = max_active.clone();
        workers.push(thread::spawn(move || {
            coordinate(
                identity("wallet", bundle_index),
                || {},
                |_| {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(current, Ordering::SeqCst);
                    both_entered.wait();
                    active.fetch_sub(1, Ordering::SeqCst);
                },
            );
        }));
    }

    both_entered.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(max_active.load(Ordering::SeqCst), 2);
}

#[test]
fn failed_leader_releases_the_waiting_retry() {
    let leader_started = Arc::new(Barrier::new(2));
    let release_leader = Arc::new((Mutex::new(false), Condvar::new()));
    let leader = {
        let leader_started = leader_started.clone();
        let release_leader = release_leader.clone();
        thread::spawn(move || {
            coordinate(
                identity("retry-wallet", 0),
                || {},
                |_| {
                    leader_started.wait();
                    let (released, wake) = &*release_leader;
                    let mut released = released.lock().unwrap();
                    while !*released {
                        released = wake.wait(released).unwrap();
                    }
                    Err::<(), _>("leader failed")
                },
            )
        })
    };
    leader_started.wait();

    let waited = Arc::new(AtomicBool::new(false));
    let follower = {
        let waited = waited.clone();
        thread::spawn(move || {
            coordinate(
                identity("retry-wallet", 0),
                || waited.store(true, Ordering::SeqCst),
                |_| Ok::<_, &str>("generated after retry"),
            )
        })
    };
    while !waited.load(Ordering::SeqCst) {
        thread::yield_now();
    }
    let (released, wake) = &*release_leader;
    *released.lock().unwrap() = true;
    wake.notify_all();

    assert_eq!(leader.join().unwrap(), Err("leader failed"));
    assert_eq!(follower.join().unwrap(), Ok("generated after retry"));
}
