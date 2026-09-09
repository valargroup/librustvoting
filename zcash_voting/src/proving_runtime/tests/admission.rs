//! Admission order is observed with held capacity, without expensive proof state.
use super::*;
use std::time::Instant;

fn queued(admission: &Admission, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while admission.queue.lock().unwrap().ready.len() != count {
        assert!(Instant::now() < deadline, "jobs did not reach admission");
        std::thread::yield_now();
    }
}

#[test]
fn bundles_receive_round_robin_admission_with_fifo_actions() {
    let admission = Admission::new(8, 1);
    let observations = crate::ObservationScope::disabled();
    let held = admission
        .acquire(&Operation::current(), &observations)
        .unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        for (index, identity) in ["A", "A", "B", "B", "C"].into_iter().enumerate() {
            let operation =
                Operation::controlled(identity.into(), crate::ChainSubmissionControl::new(0), 0);
            let send = send.clone();
            let admission = &admission;
            let observations = &observations;
            scope.spawn(move || {
                let _permit = admission.acquire(&operation, observations).unwrap();
                send.send(index).unwrap();
            });
            queued(admission, index + 1);
        }
        drop(held);
    });
    assert_eq!(receive.try_iter().collect::<Vec<_>>(), vec![0, 2, 4, 1, 3]);
}

#[test]
fn ready_queue_is_bounded_and_backpressured_waiters_can_cancel() {
    let admission = Admission::new(2, 1);
    let observations = crate::ObservationScope::disabled();
    let held = admission
        .acquire(&Operation::current(), &observations)
        .unwrap();
    let control = crate::ChainSubmissionControl::new(0);
    std::thread::scope(|scope| {
        for index in 0..3 {
            let operation = Operation::controlled(format!("bundle-{index}"), control.clone(), 0);
            let admission = &admission;
            let observations = &observations;
            scope.spawn(move || assert!(admission.acquire(&operation, observations).is_err()));
            if index < 2 {
                queued(admission, index + 1);
            }
        }
        assert_eq!(admission.queue.lock().unwrap().ready.len(), 2);
        control.cancel();
    });
    assert!(admission.queue.lock().unwrap().ready.is_empty());
    drop(held);
}
