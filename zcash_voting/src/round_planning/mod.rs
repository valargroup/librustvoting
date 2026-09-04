//! Round planning: one consistent snapshot of a round's durable state, and
//! the plan derived from it.
//!
//! The planner reads a round through [`RoundSnapshot`], loaded in one
//! deferred read transaction, and derives every step and flag from that
//! snapshot alone. The rules it applies are specified in
//! `docs/round_orchestration_invariants.md`.

mod snapshot;

pub(crate) use snapshot::{load_round_snapshot, RoundSnapshot};

#[cfg(test)]
mod tests;
