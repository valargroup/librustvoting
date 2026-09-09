-- Consecutive chain rejections of one bundle's combined delegation-and-cast
-- batch, counted against the delegation generation every recast reuses.
--
-- A terminally rejected combined batch is retired: its members, authorization
-- and lifecycle row are all deleted so the bundle reads `Proved` and can cast
-- afresh. Without this ledger nothing durable survives that retirement, so a
-- rejection caused by the delegation half re-proves every member and re-POSTs
-- the identical delegation on every run, forever.
--
-- Keyed by the delegation generation, not the batch digest: vote commitments
-- are re-randomized on every cast, so an unchanged ballot still produces a new
-- batch digest. The delegation setup is what retirement leaves untouched.
--
-- Deliberately its own table. `chain_submissions` is fingerprinted on every
-- open and rebuilt on drift, and both combined freshness gates admit a new
-- batch only while no row exists there, so the ledger must be invisible to them.
--
-- `last_batch_digest` and the two timestamps are written but never read back
-- by the SDK. They are kept because retirement destroys every other record of
-- what was refused and when, and a host debugging a stuck delegation has
-- nothing else to read; planning deliberately depends only on the streak and
-- the generation.
CREATE TABLE combined_cast_rejections (
    round_id TEXT NOT NULL,
    wallet_id TEXT NOT NULL,
    bundle_index INTEGER NOT NULL,
    delegation_generation_digest BLOB NOT NULL CHECK(length(delegation_generation_digest) = 32),
    consecutive_rejections INTEGER NOT NULL CHECK(consecutive_rejections > 0),
    last_batch_digest BLOB NOT NULL CHECK(length(last_batch_digest) = 32),
    last_diagnostic_kind TEXT NOT NULL,
    last_diagnostic TEXT NOT NULL,
    first_rejected_at INTEGER NOT NULL CHECK(first_rejected_at >= 0),
    last_rejected_at INTEGER NOT NULL CHECK(last_rejected_at >= first_rejected_at),
    PRIMARY KEY(round_id, wallet_id, bundle_index),
    FOREIGN KEY(round_id, wallet_id) REFERENCES rounds(round_id, wallet_id) ON DELETE CASCADE
);
CREATE TRIGGER combined_cast_rejections_monotonic_streak BEFORE UPDATE ON combined_cast_rejections
WHEN NEW.delegation_generation_digest = OLD.delegation_generation_digest
 AND NEW.consecutive_rejections <= OLD.consecutive_rejections
BEGIN SELECT RAISE(ABORT, 'a rejection streak cannot decrease within one delegation generation'); END;
CREATE TRIGGER combined_cast_rejections_generation_restart BEFORE UPDATE ON combined_cast_rejections
WHEN NEW.delegation_generation_digest <> OLD.delegation_generation_digest
 AND NEW.consecutive_rejections <> 1
BEGIN SELECT RAISE(ABORT, 'a new delegation generation restarts the rejection streak'); END;
