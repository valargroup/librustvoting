//! Independent, scoped evidence for a combined delegation and cast recovery unit.
//!
//! Snapshots retain public identities and fingerprints, never signing material or
//! share secrets. Assertions inspect the target bundle rather than global counts.
mod snapshot;
mod verify;

pub use snapshot::{CombinedBundle, CombinedMember};
pub use verify::{assert_combined_stage, assert_combined_terminal, assert_preserved_combined};
