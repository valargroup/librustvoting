//! Reconcile only legacy local delegation rows without submission authority.
use crate::VotingError;
use rusqlite::Connection;

pub(super) fn reconcile(conn: &Connection) -> Result<(), VotingError> {
    conn.execute_batch(LEGACY_DELEGATION_RECONCILIATION_SQL)
        .map_err(|e| {
            VotingError::from_sqlite("failed to reconcile legacy delegation proofs", &e)
        })?;
    delete_invalid_legacy_keystone_signatures(conn)?;
    conn.execute_batch(CLEAR_UNBOUND_LEGACY_DELEGATION_SETUP_SQL)
        .map_err(|e| {
            VotingError::from_sqlite("failed to clear unbound legacy delegation setup", &e)
        })
}

const LEGACY_DELEGATION_RECONCILIATION_SQL: &str =
    "-- Releases through v3.1.0-rc.15 could clear software delegation setup
-- after ZKP1 succeeded while leaving the proof row marked successful. A
-- signature written after that cleanup belongs to setup that no longer exists.
DELETE FROM keystone_signatures AS k
 WHERE EXISTS (
       SELECT 1
         FROM bundles b
        WHERE b.round_id = k.round_id
          AND b.wallet_id = k.wallet_id
          AND b.bundle_index = k.bundle_index
          AND b.delegation_pczt IS NULL
          AND NOT EXISTS (SELECT 1 FROM chain_submissions s
                           WHERE s.round_id = b.round_id AND s.wallet_id = b.wallet_id
                             AND s.bundle_index = b.bundle_index)
          AND NOT EXISTS (SELECT 1 FROM votes v
                           WHERE v.round_id = b.round_id AND v.wallet_id = b.wallet_id
                             AND v.bundle_index = b.bundle_index)
          AND NOT EXISTS (SELECT 1 FROM share_delegations v
                           WHERE v.round_id = b.round_id AND v.wallet_id = b.wallet_id
                             AND v.bundle_index = b.bundle_index)
          AND b.note_positions_blob IS NOT NULL
          AND b.delegation_tx_hash IS NULL
          AND b.van_leaf_position IS NULL
          AND (b.van_comm_rand IS NULL
               OR b.dummy_nullifiers IS NULL
               OR b.rho_signed IS NULL
               OR b.padded_note_data IS NULL
               OR b.nf_signed IS NULL
               OR b.cmx_new IS NULL
               OR b.alpha IS NULL
               OR b.rseed_signed IS NULL
               OR b.rseed_output IS NULL
               OR b.gov_comm IS NULL
               OR b.total_note_value IS NULL
               OR b.address_index IS NULL
               OR b.rk IS NULL
               OR b.gov_nullifiers_blob IS NULL
               OR b.padded_note_secrets IS NULL
               OR b.pczt_sighash IS NULL
               OR b.tx1_effects IS NULL)
   );
UPDATE proofs AS p
   SET success = 0
 WHERE p.success = 1
   AND EXISTS (
       SELECT 1
         FROM bundles b
        WHERE b.round_id = p.round_id
          AND b.wallet_id = p.wallet_id
          AND b.bundle_index = p.bundle_index
          AND b.delegation_pczt IS NULL
          AND NOT EXISTS (SELECT 1 FROM chain_submissions s
                           WHERE s.round_id = b.round_id AND s.wallet_id = b.wallet_id
                             AND s.bundle_index = b.bundle_index)
          AND NOT EXISTS (SELECT 1 FROM votes v
                           WHERE v.round_id = b.round_id AND v.wallet_id = b.wallet_id
                             AND v.bundle_index = b.bundle_index)
          AND NOT EXISTS (SELECT 1 FROM share_delegations v
                           WHERE v.round_id = b.round_id AND v.wallet_id = b.wallet_id
                             AND v.bundle_index = b.bundle_index)
          AND b.note_positions_blob IS NOT NULL
          AND b.delegation_tx_hash IS NULL
          AND b.van_leaf_position IS NULL
          AND (b.van_comm_rand IS NULL
               OR b.dummy_nullifiers IS NULL
               OR b.rho_signed IS NULL
               OR b.padded_note_data IS NULL
               OR b.nf_signed IS NULL
               OR b.cmx_new IS NULL
               OR b.alpha IS NULL
               OR b.rseed_signed IS NULL
               OR b.rseed_output IS NULL
               OR b.gov_comm IS NULL
               OR b.total_note_value IS NULL
               OR b.address_index IS NULL
               OR b.rk IS NULL
               OR b.gov_nullifiers_blob IS NULL
               OR b.padded_note_secrets IS NULL
               OR b.pczt_sighash IS NULL
               OR b.tx1_effects IS NULL)
   );
-- A signature that does not match the bundle's current signing fields can
-- block a valid replacement forever. Remove only local, unsubmitted rows.
DELETE FROM keystone_signatures
 WHERE EXISTS (
       SELECT 1
         FROM bundles b
        WHERE b.round_id = keystone_signatures.round_id
          AND b.wallet_id = keystone_signatures.wallet_id
          AND b.bundle_index = keystone_signatures.bundle_index
          AND b.delegation_pczt IS NULL
          AND NOT EXISTS (SELECT 1 FROM chain_submissions s
                           WHERE s.round_id = b.round_id AND s.wallet_id = b.wallet_id
                             AND s.bundle_index = b.bundle_index)
          AND NOT EXISTS (SELECT 1 FROM votes v
                           WHERE v.round_id = b.round_id AND v.wallet_id = b.wallet_id
                             AND v.bundle_index = b.bundle_index)
          AND NOT EXISTS (SELECT 1 FROM share_delegations v
                           WHERE v.round_id = b.round_id AND v.wallet_id = b.wallet_id
                             AND v.bundle_index = b.bundle_index)
          AND b.note_positions_blob IS NOT NULL
          AND b.delegation_tx_hash IS NULL
          AND b.van_leaf_position IS NULL
          AND (b.pczt_sighash IS NULL
               OR b.rk IS NULL
               OR keystone_signatures.sighash != b.pczt_sighash
               OR keystone_signatures.rk != b.rk)
   );
-- A successful legacy proof cannot be bound to the current randomized signing
-- context without the exact PCZT. Preserve its bytes, but demote it so ZKP1 can
-- be rebuilt. Preserve submitted, confirmed, and imported delegations.
UPDATE proofs AS p
   SET success = 0
 WHERE p.success = 1
   AND EXISTS (
       SELECT 1
         FROM bundles b
        WHERE b.round_id = p.round_id
          AND b.wallet_id = p.wallet_id
          AND b.bundle_index = p.bundle_index
          AND b.delegation_pczt IS NULL
          AND NOT EXISTS (SELECT 1 FROM chain_submissions s
                           WHERE s.round_id = b.round_id AND s.wallet_id = b.wallet_id
                             AND s.bundle_index = b.bundle_index)
          AND NOT EXISTS (SELECT 1 FROM votes v
                           WHERE v.round_id = b.round_id AND v.wallet_id = b.wallet_id
                             AND v.bundle_index = b.bundle_index)
          AND NOT EXISTS (SELECT 1 FROM share_delegations v
                           WHERE v.round_id = b.round_id AND v.wallet_id = b.wallet_id
                             AND v.bundle_index = b.bundle_index)
          AND b.note_positions_blob IS NOT NULL
          AND b.delegation_tx_hash IS NULL
          AND b.van_leaf_position IS NULL
   );";

/// Clear local setup only when no proof or signature can indicate a delegation
/// submission whose outcome was never recorded locally. Proof-bearing setup is
/// preserved after demotion because ZKP2 may still need it.
const CLEAR_UNBOUND_LEGACY_DELEGATION_SETUP_SQL: &str = "UPDATE bundles
   SET van_comm_rand = NULL,
       dummy_nullifiers = NULL,
       rho_signed = NULL,
       padded_note_data = NULL,
       nf_signed = NULL,
       cmx_new = NULL,
       alpha = NULL,
       rseed_signed = NULL,
       rseed_output = NULL,
       gov_comm = NULL,
       total_note_value = NULL,
       address_index = NULL,
       rk = NULL,
       gov_nullifiers_blob = NULL,
       padded_note_secrets = NULL,
       pczt_sighash = NULL,
       tx1_effects = NULL
 WHERE delegation_pczt IS NULL
   AND NOT EXISTS (SELECT 1 FROM chain_submissions s
                    WHERE s.round_id = bundles.round_id AND s.wallet_id = bundles.wallet_id
                      AND s.bundle_index = bundles.bundle_index)
   AND NOT EXISTS (SELECT 1 FROM votes v
                           WHERE v.round_id = bundles.round_id AND v.wallet_id = bundles.wallet_id
                             AND v.bundle_index = bundles.bundle_index)
          AND NOT EXISTS (SELECT 1 FROM share_delegations v
                           WHERE v.round_id = bundles.round_id AND v.wallet_id = bundles.wallet_id
                             AND v.bundle_index = bundles.bundle_index)
          AND note_positions_blob IS NOT NULL
   AND delegation_tx_hash IS NULL
   AND van_leaf_position IS NULL
   AND bundle_index NOT IN (
       SELECT bundle_index
         FROM proofs
        WHERE round_id = bundles.round_id
          AND wallet_id = bundles.wallet_id
          AND proof IS NOT NULL
   )
   AND bundle_index NOT IN (
       SELECT bundle_index
         FROM keystone_signatures
        WHERE round_id = bundles.round_id
          AND wallet_id = bundles.wallet_id
   );";

struct LegacyKeystoneSignatureCandidate {
    round_id: String,
    wallet_id: String,
    bundle_index: i64,
    signature: Vec<u8>,
    sighash: Vec<u8>,
    randomized_key: Vec<u8>,
    stored_sighash: Option<Vec<u8>>,
    stored_randomized_key: Option<Vec<u8>>,
}

impl LegacyKeystoneSignatureCandidate {
    fn matches_setup_and_verifies(&self) -> bool {
        self.stored_sighash.as_deref() == Some(self.sighash.as_slice())
            && self.stored_randomized_key.as_deref() == Some(self.randomized_key.as_slice())
            && super::super::operations::verify_delegation_spend_auth_signature(
                &self.randomized_key,
                &self.sighash,
                &self.signature,
            )
            .is_ok()
    }
}

fn delete_invalid_legacy_keystone_signatures(conn: &Connection) -> Result<(), VotingError> {
    let candidates = {
        let mut statement = conn
            .prepare(
                "SELECT k.round_id, k.wallet_id, k.bundle_index,
                        k.sig, k.sighash, k.rk, b.pczt_sighash, b.rk
                 FROM keystone_signatures k
                 JOIN bundles b
                   ON b.round_id = k.round_id
                  AND b.wallet_id = k.wallet_id
                  AND b.bundle_index = k.bundle_index
                 WHERE b.delegation_pczt IS NULL
                   AND NOT EXISTS (SELECT 1 FROM chain_submissions s
                                    WHERE s.round_id = b.round_id AND s.wallet_id = b.wallet_id
                                      AND s.bundle_index = b.bundle_index)
                   AND NOT EXISTS (SELECT 1 FROM votes v
                           WHERE v.round_id = b.round_id AND v.wallet_id = b.wallet_id
                             AND v.bundle_index = b.bundle_index)
          AND NOT EXISTS (SELECT 1 FROM share_delegations v
                           WHERE v.round_id = b.round_id AND v.wallet_id = b.wallet_id
                             AND v.bundle_index = b.bundle_index)
          AND b.note_positions_blob IS NOT NULL
                   AND b.delegation_tx_hash IS NULL
                   AND b.van_leaf_position IS NULL",
            )
            .map_err(|e| VotingError::Internal {
                message: format!("failed to inspect legacy Keystone signatures: {e}"),
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok(LegacyKeystoneSignatureCandidate {
                    round_id: row.get(0)?,
                    wallet_id: row.get(1)?,
                    bundle_index: row.get(2)?,
                    signature: row.get(3)?,
                    sighash: row.get(4)?,
                    randomized_key: row.get(5)?,
                    stored_sighash: row.get(6)?,
                    stored_randomized_key: row.get(7)?,
                })
            })
            .map_err(|e| VotingError::Internal {
                message: format!("failed to query legacy Keystone signatures: {e}"),
            })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| VotingError::Internal {
                message: format!("failed to read legacy Keystone signature: {e}"),
            })?
    };

    for candidate in candidates {
        if candidate.matches_setup_and_verifies() {
            continue;
        }
        conn.execute(
            "DELETE FROM keystone_signatures
             WHERE round_id = ?1 AND wallet_id = ?2 AND bundle_index = ?3",
            rusqlite::params![
                candidate.round_id,
                candidate.wallet_id,
                candidate.bundle_index
            ],
        )
        .map_err(|e| VotingError::Internal {
            message: format!(
                "failed to remove invalid legacy Keystone signature for bundle {}: {e}",
                candidate.bundle_index
            ),
        })?;
    }
    Ok(())
}
