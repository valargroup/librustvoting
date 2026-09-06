//! Killing the child, and the one channel through which anything it learned
//! survives that.
//!
//! `abort()` runs no destructor and flushes no buffer, which is the point: it
//! is the only way to model an app that was killed rather than one that shut
//! down. The cost is that every fact the parent needs must already be on disk
//! and fsynced *before* the abort, so this module owns both halves — the log
//! and the exit — and nothing else may call `abort` directly.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::stages::CrashStage;

/// One thing the child observed and the parent cannot re-derive.
///
/// Durable state answers almost every question this suite asks. These are the
/// exceptions: facts that existed only inside the killed process, such as the
/// transaction hash staging returned for a POST whose response the wallet
/// never got to classify. Without them a test could not tell an unresolved
/// dispatch from one that never happened.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Observation {
    /// The child reached the stage it was asked to crash at.
    StageReached { stage: String },
    /// A POST left for this endpoint. Recorded before dispatch, so it is
    /// present even when the response never is.
    PostDispatched { url: String },
    /// The body staging returned for a POST the wallet never classified.
    PostResponse {
        url: String,
        status: u16,
        body: String,
    },
    /// The plan the child held immediately before dying.
    PlanBeforeCrash { next_steps: Vec<String> },
}

/// Append-only record of [`Observation`]s, fsynced on every write.
///
/// Every write is flushed and fsynced immediately rather than at the end,
/// because there is no end: the process is killed. A buffered log would lose
/// exactly the last entry, which is always the interesting one.
pub struct CrashLog {
    path: PathBuf,
    file: Mutex<File>,
}

impl CrashLog {
    /// Creates or truncates the log at `path`.
    pub fn create(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    /// Reads back every observation, in the order they were recorded.
    ///
    /// A trailing partial line is ignored: the process may have been killed
    /// mid-write by something other than our own abort.
    pub fn read(path: impl AsRef<Path>) -> std::io::Result<Vec<Observation>> {
        let text = std::fs::read_to_string(path)?;
        Ok(text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect())
    }

    /// Records one observation and returns only once it is on disk.
    ///
    /// Failures are deliberately silent. This log is diagnostic scaffolding;
    /// a child that cannot write it must still crash at the stage it was told
    /// to, or the test would report a harness fault as a recovery fault.
    pub fn record(&self, observation: &Observation) {
        let Ok(mut line) = serde_json::to_string(observation) else {
            return;
        };
        line.push('\n');
        let Ok(mut file) = self.file.lock() else {
            return;
        };
        let _ = file.write_all(line.as_bytes());
        let _ = file.flush();
        let _ = file.sync_all();
    }

    /// Where this log lives.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Exit code for a child whose stage never fired.
///
/// A crash test that silently completed the round would pass every assertion
/// about durable state while proving nothing, because the state it inspected
/// was simply the finished round. Distinguishing "died where asked" from
/// "finished normally" is what stops this suite rotting into a no-op, so the
/// worker exits with this rather than zero.
pub const EXIT_STAGE_NEVER_REACHED: i32 = 70;

/// Records that `stage` was reached, then kills the process.
///
/// Never returns. The log is fsynced first; `abort` raises `SIGABRT` without
/// unwinding, so no destructor runs, no buffer is flushed, and SQLite is left
/// exactly as the last committed transaction left it.
pub fn crash_now(log: &CrashLog, stage: CrashStage) -> ! {
    log.record(&Observation::StageReached {
        stage: stage.name().to_string(),
    });
    // SAFETY: `abort` is always sound to call; it terminates the process.
    unsafe { libc::abort() }
}
