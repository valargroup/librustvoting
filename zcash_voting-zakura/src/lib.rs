//! Zakura wallet-libraries facade for Zcash shielded voting.
//!
//! The implementation lives in `zcash-voting-impl`; this crate fixes its
//! backend to the Zakura wallet-libraries family.

pub use zcash_voting_impl::*;

/// FFI-facing wire types forwarded explicitly for source-oriented generators.
pub mod wire {
    pub use zcash_voting_impl::wire::*;
}
