//! Bounded FIFO queues, with round-robin admission across bundle identities.

use super::{internal, operation::Operation};
use crate::VotingError;
use std::{
    collections::VecDeque,
    sync::{Condvar, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Queue {
    next_ticket: u64,
    ready: VecDeque<(u64, String)>,
    identities: VecDeque<String>,
    active: usize,
}

pub(super) struct Admission {
    queue: Mutex<Queue>,
    changed: Condvar,
    capacity: usize,
    maximum: usize,
}

pub(super) struct Permit<'a>(&'a Admission);
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut queue = self
            .0
            .queue
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        queue.active -= 1;
        self.0.changed.notify_all();
    }
}

impl Admission {
    pub(super) fn new(capacity: usize, maximum: usize) -> Self {
        Self {
            queue: Mutex::new(Queue::default()),
            changed: Condvar::new(),
            capacity,
            maximum,
        }
    }

    /// Waits outside the CPU pool. No expensive state is retained in this queue.
    pub(super) fn acquire(
        &self,
        operation: &Operation,
        observations: &crate::ObservationScope,
    ) -> Result<Permit<'_>, VotingError> {
        let mut queue = observations.measure("proving::queue_backpressure", || {
            let mut queue = self
                .queue
                .lock()
                .map_err(|_| internal("proving admission poisoned"))?;
            while queue.ready.len() >= self.capacity {
                operation.check()?;
                queue = self
                    .changed
                    .wait_timeout(queue, Duration::from_millis(25))
                    .map_err(|_| internal("proving admission poisoned"))?
                    .0;
            }
            operation.check()?;
            Ok(queue)
        })?;
        let queued = observations.stage("proving::ready_queue_wait");
        let ticket = queue.next_ticket;
        queue.next_ticket += 1;
        queue.ready.push_back((ticket, operation.identity.clone()));
        if !queue.identities.contains(&operation.identity) {
            queue.identities.push_back(operation.identity.clone());
        }
        loop {
            if let Err(error) = operation.check() {
                queue.ready.retain(|(queued, _)| *queued != ticket);
                if !queue
                    .ready
                    .iter()
                    .any(|(_, identity)| identity == &operation.identity)
                {
                    queue
                        .identities
                        .retain(|identity| identity != &operation.identity);
                }
                self.changed.notify_all();
                queued.finish(crate::ObservationOutcome::Cancelled, None);
                return Err(error);
            }
            let next = queue
                .identities
                .front()
                .and_then(|identity| {
                    queue
                        .ready
                        .iter()
                        .find(|(_, candidate)| candidate == identity)
                })
                .map(|(ticket, _)| *ticket);
            if queue.active < self.maximum && next == Some(ticket) {
                queue.ready.retain(|(queued, _)| *queued != ticket);
                let identity = queue
                    .identities
                    .pop_front()
                    .expect("admitted identity exists");
                if queue
                    .ready
                    .iter()
                    .any(|(_, candidate)| candidate == &identity)
                {
                    queue.identities.push_back(identity);
                }
                queue.active += 1;
                self.changed.notify_all();
                queued.finish(crate::ObservationOutcome::Succeeded, None);
                return Ok(Permit(self));
            }
            queue = self
                .changed
                .wait_timeout(queue, Duration::from_millis(25))
                .map_err(|_| internal("proving admission poisoned"))?
                .0;
        }
    }
}

#[cfg(test)]
#[path = "tests/admission.rs"]
mod tests;
