//! The inter-pass repoll wait observes host cancellation promptly.

use std::time::Duration;

use super::{interrupted_during, ChainSubmissionControl};

#[tokio::test(start_paused = true)]
async fn a_cancellation_during_the_repoll_wait_returns_promptly() {
    let control = ChainSubmissionControl::new(1);
    let waiter = {
        let control = control.clone();
        tokio::spawn(
            async move { interrupted_during(Duration::from_secs(3_600), &control, 1).await },
        )
    };
    let started = tokio::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !waiter.is_finished(),
        "the wait must not end before cancellation"
    );

    control.cancel();
    let cancelled = waiter.await.unwrap();

    assert!(cancelled);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "cancellation took {:?} to be observed",
        started.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn an_uncancelled_repoll_wait_lasts_the_full_delay() {
    let control = ChainSubmissionControl::new(1);
    let started = tokio::time::Instant::now();

    let cancelled = interrupted_during(Duration::from_secs(90), &control, 1).await;

    assert!(!cancelled);
    assert_eq!(started.elapsed(), Duration::from_secs(90));
}

#[tokio::test(start_paused = true)]
async fn an_already_cancelled_control_skips_the_wait() {
    let control = ChainSubmissionControl::new(1);
    control.cancel();
    let started = tokio::time::Instant::now();

    assert!(interrupted_during(Duration::from_secs(90), &control, 1).await);
    assert_eq!(started.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn an_unrepresentable_repoll_deadline_waits_for_cancellation_instead_of_panicking() {
    let control = ChainSubmissionControl::new(1);
    let waiter = {
        let control = control.clone();
        tokio::spawn(async move { interrupted_during(Duration::MAX, &control, 1).await })
    };
    tokio::time::sleep(Duration::from_secs(3_600)).await;
    assert!(!waiter.is_finished(), "an unbounded repoll keeps waiting");

    control.cancel();

    assert!(waiter.await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn an_operation_epoch_change_during_the_repoll_wait_interrupts_it() {
    let control = ChainSubmissionControl::new(1);
    let waiter = {
        let control = control.clone();
        tokio::spawn(
            async move { interrupted_during(Duration::from_secs(3_600), &control, 1).await },
        )
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!waiter.is_finished());

    control.set_operation_epoch(2);
    let interrupted = waiter.await.unwrap();

    assert!(interrupted);
    assert!(
        !control.is_cancelled(),
        "an epoch change is not a cancellation"
    );
}
