//! Driver-level behaviour: why a tracking run stops, and how it paces itself.
//!
//! What one pass *does* to helper traffic and share rows belongs to
//! [`share_tracking`](crate::share_tracking)'s own tests. These cover the
//! layer above: the loop, its stop reasons, and the policy that governs a
//! failing pass. They reach no network — a share whose status check is still
//! in the future gives a real multi-pass run with nothing to poll.

mod fixtures;
mod pacing;
mod stopping;
