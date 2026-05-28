//! Durable ballot intent + resumable voting-session planner.
//!
//! `resume_plan` is pure and I/O-free over the wallet's voting DB: it reports
//! the ordered remaining work for a round, built on the per-artifact phase
//! APIs in `crate::phases`. The wallet executes each step with its own
//! network/proof/sign plumbing.

use rusqlite::named_params;

use crate::storage::VotingDb;
use crate::types::VotingError;

/// The voter's terminal decision for one proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Choice(u32),
    Skipped,
}

impl VotingDb {
    /// Record (insert or replace) the voter's decision for one proposal.
    /// Written on each selection, before any per-proposal vote artifact exists.
    pub fn set_ballot_intent(
        &self,
        round_id: &str,
        proposal_id: u32,
        decision: Decision,
    ) -> Result<(), VotingError> {
        let (skipped, choice): (i64, Option<i64>) = match decision {
            Decision::Choice(c) => (0, Some(c as i64)),
            Decision::Skipped => (1, None),
        };
        let now = now_secs();
        let conn = self.conn();
        let wallet_id = self.wallet_id();
        conn.execute(
            "INSERT INTO ballot_intent
                (round_id, wallet_id, proposal_id, skipped, choice, created_at, updated_at)
             VALUES (:round_id, :wallet_id, :proposal_id, :skipped, :choice, :now, :now)
             ON CONFLICT(round_id, wallet_id, proposal_id)
             DO UPDATE SET skipped = :skipped, choice = :choice, updated_at = :now",
            named_params! {
                ":round_id": round_id,
                ":wallet_id": wallet_id,
                ":proposal_id": proposal_id as i64,
                ":skipped": skipped,
                ":choice": choice,
                ":now": now,
            },
        )
        .map_err(|e| VotingError::Internal { message: format!("set_ballot_intent failed: {e}") })?;
        Ok(())
    }

    /// Load the voter's decisions for a round, sorted by proposal id.
    pub fn ballot_intents(&self, round_id: &str) -> Result<Vec<(u32, Decision)>, VotingError> {
        let conn = self.conn();
        let wallet_id = self.wallet_id();
        let mut stmt = conn
            .prepare(
                "SELECT proposal_id, skipped, choice FROM ballot_intent
                 WHERE round_id = :round_id AND wallet_id = :wallet_id
                 ORDER BY proposal_id",
            )
            .map_err(|e| VotingError::Internal { message: format!("prepare ballot_intents: {e}") })?;
        let rows = stmt
            .query_map(
                named_params! { ":round_id": round_id, ":wallet_id": wallet_id },
                |row| {
                    let pid = row.get::<_, i64>(0)? as u32;
                    let skipped: i64 = row.get(1)?;
                    let choice: Option<i64> = row.get(2)?;
                    let decision = if skipped != 0 {
                        Decision::Skipped
                    } else {
                        Decision::Choice(choice.unwrap_or(0) as u32)
                    };
                    Ok((pid, decision))
                },
            )
            .map_err(|e| VotingError::Internal { message: format!("query ballot_intents: {e}") })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| VotingError::Internal { message: format!("collect ballot_intents: {e}") })
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::round::RoundParams;
    use crate::types::NoteInfo;

    const ROUND: &str = "0101010101010101010101010101010101010101010101010101010101010101";
    const W: &str = "wallet";

    fn round_params() -> RoundParams {
        RoundParams {
            vote_round_id: ROUND.to_string(),
            snapshot_height: 1000,
            ea_pk: vec![0xEA; 32],
            nc_root: vec![0xAA; 32],
            nullifier_imt_root: vec![0xBB; 32],
        }
    }

    fn note(position: u64) -> NoteInfo {
        NoteInfo {
            commitment: vec![0x01; 32],
            nullifier: vec![0x02; 32],
            value: crate::governance::BALLOT_DIVISOR,
            position,
            diversifier: vec![0x03; 11],
            rho: vec![0x04; 32],
            rseed: vec![0x05; 32],
            scope: 0,
            ufvk_str: "uview1test".to_string(),
        }
    }

    fn db_with_bundle() -> VotingDb {
        let db = VotingDb::open_in_memory().unwrap();
        db.set_wallet_id(W);
        db.create_round(&round_params()).unwrap();
        db.ensure_bundles(ROUND, &[note(0)]).unwrap();
        db
    }

    #[test]
    fn ballot_intent_round_trip() {
        let db = db_with_bundle();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(0)).unwrap();
        db.set_ballot_intent(ROUND, 2, Decision::Skipped).unwrap();
        db.set_ballot_intent(ROUND, 1, Decision::Choice(3)).unwrap(); // upsert

        let intents = db.ballot_intents(ROUND).unwrap();
        assert_eq!(intents, vec![(1, Decision::Choice(3)), (2, Decision::Skipped)]);
    }
}
