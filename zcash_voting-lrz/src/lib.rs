//! Upstream librustzcash facade for Zcash shielded voting.
//!
//! The implementation lives in `zcash_voting`; this crate fixes its backend
//! to the upstream librustzcash family.

pub use zcash_voting_impl::*;

/// FFI-facing wire types forwarded explicitly for source-oriented generators.
pub mod wire {
    pub use zcash_voting_impl::wire::*;
}
