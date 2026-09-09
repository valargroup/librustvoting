//! CPU closures execute with admission held; callers wait outside the pool.

use super::{internal, runtime, Operation};
use crate::{ObservationScope, VotingError};
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::mpsc,
};

/// Executes one CPU job; nested admission on a worker is forbidden.
pub(crate) fn execute<R: Send>(
    observations: &ObservationScope,
    compute: impl FnOnce() -> Result<R, VotingError> + Send,
) -> Result<R, VotingError> {
    runtime().map_err(internal)?.execute(observations, compute)
}

impl super::Runtime {
    pub(super) fn execute<R: Send>(
        &self,
        observations: &ObservationScope,
        compute: impl FnOnce() -> Result<R, VotingError> + Send,
    ) -> Result<R, VotingError> {
        let runtime = self;
        if runtime.pool.current_thread_index().is_some() {
            return Err(internal("recursive proving admission on a CPU worker"));
        }
        let operation = Operation::current();
        let permit = observations.measure_result("proving::admission_wait", || {
            runtime.admission.acquire(&operation, observations)
        })?;
        runtime.pool.install(move || {
            let _permit = permit;
            operation.check()?;
            observations.measure_result("proving::heavy_job", || {
                catch_unwind(AssertUnwindSafe(compute))
                    .map_err(|_| internal("proving CPU job panicked"))?
            })
        })
    }
}

/// Executes a locally bounded batch, returning canonical input order.
/// The caller owns the scope and waits for completions without occupying a worker.
pub(crate) fn execute_many<R: Send>(
    count: usize,
    maximum: usize,
    observations: &ObservationScope,
    compute: impl Fn(usize) -> Result<R, VotingError> + Sync,
) -> Result<Vec<R>, VotingError> {
    runtime()
        .map_err(internal)?
        .execute_many(count, maximum, observations, compute)
}

impl super::Runtime {
    pub(super) fn execute_many<R: Send>(
        &self,
        count: usize,
        maximum: usize,
        observations: &ObservationScope,
        compute: impl Fn(usize) -> Result<R, VotingError> + Sync,
    ) -> Result<Vec<R>, VotingError> {
        let runtime = self;
        if runtime.pool.current_thread_index().is_some() {
            return Err(internal("batch orchestration on a CPU worker"));
        }
        let operation = Operation::current().child();
        let (send, receive) = mpsc::channel();
        let mut results: Vec<Option<Result<R, VotingError>>> = (0..count).map(|_| None).collect();
        runtime.pool.in_place_scope(|scope| {
            let mut next = 0;
            let mut active = 0;
            let mut failure = None;
            loop {
                while failure.is_none() && next < count && active < maximum.max(1) {
                    let permit = match observations
                        .measure_result("proving::admission_wait", || {
                            runtime.admission.acquire(&operation, observations)
                        }) {
                        Ok(permit) => permit,
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    };
                    let index = next;
                    next += 1;
                    active += 1;
                    let send = send.clone();
                    let compute = &compute;
                    let operation = operation.clone();
                    scope.spawn(move |_| {
                        let result = {
                            let _permit = permit;
                            let result = operation.check().and_then(|()| {
                                observations.measure_result("proving::heavy_job", || {
                                    catch_unwind(AssertUnwindSafe(|| compute(index)))
                                        .map_err(|_| internal("proving CPU job panicked"))?
                                })
                            });
                            if result.is_err() {
                                operation.cancel();
                            }
                            result
                        };
                        let _ = send.send((index, result));
                    });
                }
                if active == 0 {
                    break;
                }
                let (index, result) = receive
                    .recv()
                    .expect("scoped proof always reports completion");
                active -= 1;
                if result.is_err() && failure.is_none() {
                    failure = Some(internal("atomic proof preparation failed"));
                }
                results[index] = Some(result);
            }
            // Prefer the canonical action's concrete error over the stop marker.
            if results.iter().any(|result| matches!(result, Some(Err(_)))) {
                return Ok(());
            }
            if let Some(error) = failure {
                return Err(error);
            }
            Ok(())
        })?;
        let concrete_error = results.iter().position(|result| match result {
            Some(Err(error)) => !super::operation::is_interruption(error),
            _ => false,
        });
        let first_error = concrete_error.or_else(|| {
            results
                .iter()
                .position(|result| matches!(result, Some(Err(_))))
        });
        if let Some(index) = first_error {
            return Err(results[index]
                .take()
                .expect("completed error")
                .err()
                .expect("failed proof"));
        }
        operation.check()?;
        results
            .into_iter()
            .map(|result| result.ok_or_else(|| internal("atomic proof result missing"))?)
            .collect()
    }
}

/// Runs blocking orchestration outside CPU workers, including for hosts that
/// poll the SDK's asynchronous delegation API without a Tokio runtime.
pub(crate) fn spawn_orchestration(work: impl FnOnce() + Send + 'static) -> Result<(), VotingError> {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            runtime.spawn_blocking(work);
        }
        Err(_) => {
            std::thread::Builder::new()
                .name("voting-orchestration".into())
                .spawn(work)
                .map_err(|error| {
                    internal(format!("could not start voting orchestration: {error}"))
                })?;
        }
    }
    Ok(())
}
