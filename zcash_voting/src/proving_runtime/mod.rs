//! Process-wide admission and CPU execution for voting proofs and proving keys.
//! Orchestration stays on callers; only admitted CPU closures enter the pool.

mod admission;
mod cache_initialization;
mod execution;
mod operation;

use std::{num::NonZeroUsize, sync::OnceLock};

pub(crate) use cache_initialization::{ensure_cache, CacheKind};
pub(crate) use execution::{execute, execute_many, spawn_orchestration};
pub(crate) use operation::{check_interruption, Operation};

/// Independent CPU and memory-pressure limits for all SDK proving callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProvingPolicy {
    /// Number of shared Rayon workers, each with a 64 MiB stack.
    pub cpu_worker_count: NonZeroUsize,
    /// Maximum admitted proof/key-generation jobs, including dispatched roots.
    pub max_active_heavy_jobs: NonZeroUsize,
}

impl Default for ProvingPolicy {
    fn default() -> Self {
        let parallelism = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
        Self {
            cpu_worker_count: parallelism,
            max_active_heavy_jobs: parallelism,
        }
    }
}

/// Failure to establish the immutable process-wide proving configuration.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProvingConfigurationError {
    /// A successful configuration or automatic first use fixed another policy.
    #[error("proving runtime is already configured with a different policy")]
    AlreadyConfigured,
    /// Twice the requested worker count cannot be represented.
    #[error("proving runtime queue capacity overflows usize")]
    QueueCapacityOverflow,
    /// The operating system could not create the requested CPU pool.
    #[error("could not initialize proving runtime: {0}")]
    PoolInitialization(String),
}

static POLICY: OnceLock<ProvingPolicy> = OnceLock::new();
static RUNTIME: OnceLock<Result<Runtime, ProvingConfigurationError>> = OnceLock::new();

/// Fixes the process policy before first proving or warm-up use.
/// Identical repeats succeed. Conflicts never change a running pool.
pub fn configure_proving_runtime(policy: ProvingPolicy) -> Result<(), ProvingConfigurationError> {
    policy
        .cpu_worker_count
        .get()
        .checked_mul(2)
        .ok_or(ProvingConfigurationError::QueueCapacityOverflow)?;
    if *POLICY.get_or_init(|| policy) != policy {
        return Err(ProvingConfigurationError::AlreadyConfigured);
    }
    runtime().map(|_| ())
}

struct Runtime {
    pool: rayon::ThreadPool,
    admission: admission::Admission,
}

impl Runtime {
    fn new(policy: ProvingPolicy) -> Result<Self, ProvingConfigurationError> {
        let capacity = policy
            .cpu_worker_count
            .get()
            .checked_mul(2)
            .ok_or(ProvingConfigurationError::QueueCapacityOverflow)?;
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(policy.cpu_worker_count.get())
            .stack_size(64 * 1024 * 1024)
            .thread_name(|index| format!("voting-prover-{index}"))
            .build()
            .map_err(|error| ProvingConfigurationError::PoolInitialization(error.to_string()))?;
        Ok(Self {
            pool,
            admission: admission::Admission::new(capacity, policy.max_active_heavy_jobs.get()),
        })
    }
}

fn runtime() -> Result<&'static Runtime, ProvingConfigurationError> {
    RUNTIME
        .get_or_init(|| Runtime::new(*POLICY.get_or_init(ProvingPolicy::default)))
        .as_ref()
        .map_err(Clone::clone)
}

fn internal(message: impl ToString) -> crate::VotingError {
    crate::VotingError::Internal {
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests;
