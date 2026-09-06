//! Crashing a run, and reading back what the crash left behind.
//!
//! The child half arms one crash point and dies at it. The parent half spawns
//! that child, proves it died where it was told to, and hands the test the
//! sidecar it left. Nothing here interprets durable state; that is
//! [`assertions`](crate::assertions).
//!
//! Why a separate process at all: both provers run on dedicated OS threads
//! that are deliberately not cancellable, and they hold the round lock through
//! a cloned `Arc` so it outlives a dropped future. An in-process crash model
//! would leave a live prover still holding that lock and still writing to the
//! sidecar the "restarted" run had just reopened, quietly corrupting the state
//! under test. Killing the process is what makes the detached prover go away.

mod crash;
mod crash_helper_transport;
mod crash_reporter;
mod crash_transport;
mod spawn;

pub use crash::{CrashLog, Observation, EXIT_STAGE_NEVER_REACHED};
pub use crash_helper_transport::CrashHelperTransport;
pub use crash_reporter::{CrashReporter, CrashTarget};
pub use crash_transport::CrashTransport;
pub use spawn::{run_until_crash, CrashOutcome, CrashRun};
