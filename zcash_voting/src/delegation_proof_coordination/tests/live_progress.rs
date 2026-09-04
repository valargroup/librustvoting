//! Live progress delivery: the producer never waits on the host, and the host
//! still sees each event while the operation is running.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    delegate::DelegationProgress,
    types::{DelegationProgressBridge, DelegationProgressReporter},
};

use super::super::with_live_progress;

const DEADLINE: Duration = Duration::from_secs(30);

fn spin_until(flag: &AtomicBool) {
    let started = Instant::now();
    while !flag.load(Ordering::SeqCst) {
        assert!(started.elapsed() < DEADLINE, "flag never set");
        thread::yield_now();
    }
}

#[test]
fn progress_reaches_the_host_while_the_operation_is_still_running() {
    let host_saw_start = Arc::new(AtomicBool::new(false));
    let host = {
        let host_saw_start = Arc::clone(&host_saw_start);
        DelegationProgressBridge::new(move |event| {
            if event == DelegationProgress::ProofStarting {
                host_saw_start.store(true, Ordering::SeqCst);
            }
        })
    };

    let (done, finished) = mpsc::channel();
    let flag = Arc::clone(&host_saw_start);
    thread::spawn(move || {
        let outcome = with_live_progress(&host, |progress| {
            progress.on_progress(DelegationProgress::ProofStarting);
            // Deferred delivery would never satisfy this while the operation
            // is still inside the closure.
            spin_until(&flag);
            "operation observed live delivery"
        });
        let _ = done.send(outcome);
    });

    let outcome = finished
        .recv_timeout(DEADLINE)
        .expect("progress was not delivered until after the operation returned");
    assert_eq!(outcome, "operation observed live delivery");
}

#[test]
fn blocked_host_callback_does_not_stall_the_producer() {
    let release_host = Arc::new(AtomicBool::new(false));
    let host_entered = Arc::new(AtomicBool::new(false));
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let host = {
        let release_host = Arc::clone(&release_host);
        let host_entered = Arc::clone(&host_entered);
        let delivered = Arc::clone(&delivered);
        DelegationProgressBridge::new(move |event| {
            host_entered.store(true, Ordering::SeqCst);
            spin_until(&release_host);
            delivered.lock().unwrap().push(event);
        })
    };

    let operation_returned = Arc::new(AtomicBool::new(false));
    let (done, finished) = mpsc::channel();
    {
        let host_entered = Arc::clone(&host_entered);
        let operation_returned = Arc::clone(&operation_returned);
        thread::spawn(move || {
            with_live_progress(&host, |progress| {
                progress.on_progress(DelegationProgress::ProofStarting);
                progress.on_progress(DelegationProgress::ProofProgress(0.5));
                progress.on_progress(DelegationProgress::ProofComplete);
                spin_until(&host_entered);
                operation_returned.store(true, Ordering::SeqCst);
            });
            let _ = done.send(());
        });
    }

    // The operation finished while the host was still blocked in its first
    // callback, so the producer never waited on the host.
    spin_until(&operation_returned);
    assert!(
        finished.try_recv().is_err(),
        "delivery must complete before return"
    );
    release_host.store(true, Ordering::SeqCst);
    finished
        .recv_timeout(DEADLINE)
        .expect("delivery thread did not drain after release");

    assert_eq!(
        *delivered.lock().unwrap(),
        vec![
            DelegationProgress::ProofStarting,
            DelegationProgress::ProofProgress(0.5),
            DelegationProgress::ProofComplete,
        ]
    );
}

#[test]
fn every_event_is_delivered_in_order_before_return() {
    const EVENTS: usize = 500;
    let delivered = Arc::new(Mutex::new(Vec::with_capacity(EVENTS)));
    let host = {
        let delivered = Arc::clone(&delivered);
        DelegationProgressBridge::new(move |event| delivered.lock().unwrap().push(event))
    };

    with_live_progress(&host, |progress| {
        for step in 0..EVENTS {
            progress.on_progress(DelegationProgress::ProofProgress(
                step as f64 / EVENTS as f64,
            ));
        }
    });

    let delivered = delivered.lock().unwrap();
    assert_eq!(delivered.len(), EVENTS);
    for (step, event) in delivered.iter().enumerate() {
        assert_eq!(
            *event,
            DelegationProgress::ProofProgress(step as f64 / EVENTS as f64)
        );
    }
}

#[test]
fn host_callbacks_never_run_on_the_operation_thread() {
    let operation_thread = Arc::new(Mutex::new(None));
    let callback_threads = Arc::new(Mutex::new(Vec::new()));
    let callback_count = Arc::new(AtomicUsize::new(0));
    let host = {
        let callback_threads = Arc::clone(&callback_threads);
        let callback_count = Arc::clone(&callback_count);
        DelegationProgressBridge::new(move |_| {
            callback_threads
                .lock()
                .unwrap()
                .push(thread::current().id());
            callback_count.fetch_add(1, Ordering::SeqCst);
        })
    };

    with_live_progress(&host, |progress| {
        *operation_thread.lock().unwrap() = Some(thread::current().id());
        progress.on_progress(DelegationProgress::WaitingForExistingProof);
        progress.on_progress(DelegationProgress::ProofComplete);
    });

    let operation_thread = operation_thread.lock().unwrap().unwrap();
    assert_eq!(callback_count.load(Ordering::SeqCst), 2);
    for callback_thread in callback_threads.lock().unwrap().iter() {
        assert_ne!(*callback_thread, operation_thread);
    }
}

#[test]
fn operation_without_progress_returns_promptly() {
    struct PanickingHost;
    impl DelegationProgressReporter for PanickingHost {
        fn on_progress(&self, _: DelegationProgress) {
            panic!("no progress should be delivered");
        }
    }

    assert_eq!(with_live_progress(&PanickingHost, |_| 7), 7);
}
