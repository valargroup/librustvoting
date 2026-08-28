//! Selected wallet and proving crates.
//!
//! `zcash_voting` depends directly on the upstream librustzcash family, so
//! the rest of the crate can just `use zcash_client_backend::...` etc. The
//! Zakura family is built from this same source (`src/lib.rs`) by the
//! sibling `zcash_voting-zakura` crate (never published), whose manifest
//! renames its Zakura deps to these same extern names via `package = "..."`.
//! This module re-exports them under one name so the rest of the crate stays
//! agnostic to which manifest is compiling it.

pub use voting_crypto_deps::{halo2_gadgets, halo2_proofs, pasta_curves};
pub use {orchard, pczt, zcash_client_backend, zcash_client_sqlite, zcash_keys, zcash_primitives};
