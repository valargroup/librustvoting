//! Selected wallet and proving crates.
//!
//! Exactly one crate feature selects the concrete dependency family. This
//! module re-exports that family's crates under stable internal names so the
//! rest of the implementation remains backend-agnostic.

pub use voting_crypto_deps::{halo2_gadgets, halo2_proofs, pasta_curves};

#[cfg(feature = "upstream")]
pub use {orchard, pczt, zcash_client_backend, zcash_client_sqlite, zcash_keys, zcash_primitives};

#[cfg(feature = "zakura")]
pub use {
    zakura_client_backend as zcash_client_backend, zakura_client_sqlite as zcash_client_sqlite,
    zakura_keys as zcash_keys, zakura_orchard as orchard, zakura_pczt as pczt,
    zakura_primitives as zcash_primitives,
};
