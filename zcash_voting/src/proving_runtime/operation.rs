//! Captured identity and interruption across blocking orchestration boundaries.

use crate::{ChainSubmissionControl, VotingError};
use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

static NEXT_INVOCATION: AtomicU64 = AtomicU64::new(1);
thread_local! { static CURRENT: RefCell<Option<Operation>> = const { RefCell::new(None) }; }

/// A lightweight operation context; no proof state or database guards.
#[derive(Clone)]
pub(crate) struct Operation {
    pub(super) identity: String,
    cancelled: Vec<Arc<AtomicBool>>,
    control: Option<(ChainSubmissionControl, u64)>,
}

/// Marks detached orchestration interrupted when its owner stops waiting.
pub(crate) struct OperationOwner(Operation);
impl Drop for OperationOwner {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Operation {
    pub(crate) fn current() -> Self {
        CURRENT
            .with(|current| current.borrow().clone())
            .unwrap_or_else(|| Self {
                identity: format!(
                    "invocation:{}",
                    NEXT_INVOCATION.fetch_add(1, Ordering::Relaxed)
                ),
                cancelled: vec![Arc::new(AtomicBool::new(false))],
                control: None,
            })
    }

    pub(crate) fn controlled(
        identity: String,
        control: ChainSubmissionControl,
        epoch: u64,
    ) -> Self {
        Self {
            identity,
            cancelled: vec![Arc::new(AtomicBool::new(false))],
            control: Some((control, epoch)),
        }
    }

    pub(crate) fn for_bundle(db: &crate::round::VotingDb, round: &str, bundle: u32) -> Self {
        let mut operation = Self::current();
        operation.identity = format!(
            "{}:{}:{}:{}",
            db.sidecar_id(),
            db.wallet_id(),
            round,
            bundle
        );
        operation
    }

    pub(super) fn child(&self) -> Self {
        let mut child = self.clone();
        child.cancelled.push(Arc::new(AtomicBool::new(false)));
        child
    }

    pub(super) fn cancel(&self) {
        self.cancelled
            .last()
            .expect("operation owns cancellation")
            .store(true, Ordering::Release);
    }

    pub(crate) fn owner(&self) -> OperationOwner {
        OperationOwner(self.clone())
    }

    pub(crate) fn check(&self) -> Result<(), VotingError> {
        if self
            .cancelled
            .iter()
            .any(|flag| flag.load(Ordering::Acquire))
            || self.control.as_ref().is_some_and(|(control, epoch)| {
                control.is_cancelled() || control.operation_epoch() != *epoch
            })
        {
            Err(super::internal("proving operation interrupted"))
        } else {
            Ok(())
        }
    }

    /// Restores the previous scope even if host code unwinds.
    pub(crate) fn enter<R>(&self, execute: impl FnOnce() -> R) -> R {
        struct Restore(Option<Operation>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CURRENT.with(|current| current.replace(self.0.take()));
            }
        }
        let _restore = Restore(CURRENT.with(|current| current.replace(Some(self.clone()))));
        execute()
    }
}

pub(crate) fn check_interruption() -> Result<(), VotingError> {
    Operation::current().check()
}

/// Distinguishes cancellation from concrete proof failures when draining siblings.
pub(super) fn is_interruption(error: &VotingError) -> bool {
    matches!(error, VotingError::Internal { message } if message == "proving operation interrupted")
}
