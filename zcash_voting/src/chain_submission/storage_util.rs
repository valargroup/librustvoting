//! Shared SQLite and timestamp adapters for durable submission state.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::types::VotingError;

pub(super) fn now_seconds() -> Result<i64, VotingError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| VotingError::Internal {
            message: format!("system clock before Unix epoch: {error}"),
        })?
        .as_secs();
    i64::try_from(seconds).map_err(|_| VotingError::Internal {
        message: "current Unix time does not fit in SQLite integer".to_string(),
    })
}

pub(super) fn internal(
    context: &'static str,
) -> impl FnOnce(rusqlite::Error) -> VotingError + Copy {
    move |error| VotingError::Internal {
        message: format!("{context} failed: {error}"),
    }
}
