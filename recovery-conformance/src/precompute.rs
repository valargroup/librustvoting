//! Reusable PIR fixtures for the disposable conformance wallet.

use anyhow::{Context, Result};

/// Whether to import cached proofs while preserving the template's dummy nullifiers.
#[derive(Clone, Copy, Debug)]
pub enum ProofCacheSeed {
    /// Leave proofs absent so the armed PIR route must observe a real request.
    Cold,
    /// Import snapshot-bound cached proofs for normal execution and recovery.
    Warm,
}

/// What a template supplied.
pub struct SeededPrecompute {
    pub proofs: usize,
    pub padded_bundles: usize,
}

/// Carries a previous round's precompute into this one.
///
/// Padding is copied only where absent in the requested wallet/round bundles;
/// existing padding is never changed. `Cold` omits the proof import, so a fresh
/// fault sidecar still needs network I/O; `Warm` can import proofs on resume.
/// Two fixture components are shared, and the second makes the first useful:
///
/// - `pir_proof_cache` rows, keyed by `(wallet, network, tree root, nullifier)`
///   and therefore **not** round-specific: a proof fetched for one round is
///   valid for any round over the same snapshot.
/// - `bundles.padded_note_secrets`, which decide the dummy nullifiers a bundle
///   pads its slots with. `ensure_padded_secrets` samples them only when
///   absent, so seeding them makes this round pad with the same nullifiers the
///   cached proofs were fetched for. Without this the cache misses every padded
///   slot and the run refetches them — which is exactly what an earlier version
///   of this suite did, seeding proofs that could never be hit.
///
/// The privacy cost is real and bounded to this harness: padding exists so an
/// observer cannot tell a bundle's real notes from its dummies, and reusing
/// dummies across rounds lets one correlate them. That is acceptable only for a
/// disposable test wallet whose seed is shared with a test suite, and must
/// never be done for a wallet holding anything.
pub fn seed_precompute(
    sidecar: &std::path::Path,
    template: &std::path::Path,
    round_id: &str,
    proof_cache: ProofCacheSeed,
) -> Result<SeededPrecompute> {
    let connection = rusqlite::Connection::open(sidecar).context("opening the sidecar")?;
    connection
        .execute(
            "ATTACH DATABASE ?1 AS warm",
            rusqlite::params![template.to_str().context("template path is not UTF-8")?],
        )
        .context("attaching the template")?;

    let proofs = match proof_cache {
        ProofCacheSeed::Cold => 0,
        ProofCacheSeed::Warm => connection
            .execute(
                "INSERT OR IGNORE INTO pir_proof_cache SELECT * FROM warm.pir_proof_cache",
                [],
            )
            .context("copying cached PIR proofs")?,
    };

    // Matched by bundle index within the wallet: bundle rows are round-keyed,
    // so the secrets move across rounds rather than being copied wholesale.
    let padded_bundles = connection
        .execute(
            "UPDATE bundles SET padded_note_secrets = (
                 SELECT w.padded_note_secrets FROM warm.bundles w
                 WHERE w.bundle_index = bundles.bundle_index
                   AND w.wallet_id = bundles.wallet_id
                   AND w.padded_note_secrets IS NOT NULL
             )
             WHERE round_id = ?1
               AND padded_note_secrets IS NULL
               AND EXISTS (
                 SELECT 1 FROM warm.bundles w
                 WHERE w.bundle_index = bundles.bundle_index
                   AND w.wallet_id = bundles.wallet_id
                   AND w.padded_note_secrets IS NOT NULL
               )",
            rusqlite::params![round_id],
        )
        .context("copying padded-slot secrets")?;

    let _ = connection.execute("DETACH DATABASE warm", []);
    Ok(SeededPrecompute {
        proofs,
        padded_bundles,
    })
}
