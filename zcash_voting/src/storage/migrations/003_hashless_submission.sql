DROP TRIGGER chain_submissions_immutable_identity;
DROP TRIGGER chain_submissions_monotonic_reservations;
DROP TRIGGER chain_submissions_immutable_tracking_start;
DROP INDEX chain_submissions_identity;
DROP INDEX chain_submissions_candidate_owner;
DROP INDEX chain_submissions_confirmation_hash_owner;
ALTER TABLE chain_submissions RENAME TO chain_submissions_v18;

CREATE TABLE chain_submissions (
    identity_key BLOB NOT NULL PRIMARY KEY,
    round_id TEXT NOT NULL,
    wallet_id TEXT NOT NULL DEFAULT '',
    network TEXT NOT NULL CHECK (network IN ('mainnet','testnet','regtest')),
    bundle_index INTEGER NOT NULL CHECK (bundle_index BETWEEN 0 AND 4294967295),
    kind TEXT NOT NULL CHECK (kind IN ('delegation','vote','vote_batch')),
    proposal_id INTEGER,
    ordered_batch_digest BLOB,
    generation_digest BLOB NOT NULL CHECK (length(generation_digest) = 32),
    state TEXT NOT NULL CHECK (state IN ('submitting','tracking','recovering','submitted_without_hash','confirmed','rejected')),
    candidate_transaction_hash BLOB,
    committed_post_reservations INTEGER NOT NULL DEFAULT 0 CHECK (committed_post_reservations >= 0),
    tracking_started_at INTEGER,
    diagnostic_kind TEXT,
    diagnostic TEXT,
    confirmation_source TEXT CHECK (confirmation_source IN ('hash','tree')),
    confirmed_transaction_hash BLOB,
    final_van_position INTEGER,
    vote_commitment_positions BLOB,
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= created_at),
    FOREIGN KEY (round_id, wallet_id) REFERENCES rounds(round_id, wallet_id) ON DELETE CASCADE,
    CHECK (length(identity_key) >= 32),
    CHECK ((kind = 'delegation' AND proposal_id IS NULL AND ordered_batch_digest IS NULL)
        OR (kind = 'vote' AND proposal_id BETWEEN 1 AND 50 AND ordered_batch_digest IS NULL)
        OR (kind = 'vote_batch' AND proposal_id IS NULL AND length(ordered_batch_digest) = 32)),
    CHECK (candidate_transaction_hash IS NULL OR length(candidate_transaction_hash) = 32),
    CHECK (confirmed_transaction_hash IS NULL OR length(confirmed_transaction_hash) = 32),
    CHECK ((state = 'submitting' AND candidate_transaction_hash IS NULL AND tracking_started_at IS NULL)
        OR (state = 'tracking' AND candidate_transaction_hash IS NOT NULL AND tracking_started_at IS NOT NULL)
        OR state IN ('recovering','submitted_without_hash','confirmed','rejected')),
    CHECK (state != 'submitted_without_hash'
        OR (candidate_transaction_hash IS NULL
            AND confirmed_transaction_hash IS NULL AND final_van_position IS NULL
            AND vote_commitment_positions IS NULL AND diagnostic_kind IS NOT NULL)),
    CHECK ((diagnostic_kind IS NULL) = (diagnostic IS NULL)),
    CHECK (diagnostic IS NULL OR length(CAST(diagnostic AS BLOB)) <= 512),
    CHECK ((state = 'confirmed') = (confirmation_source IS NOT NULL)),
    CHECK (state != 'confirmed'
        OR (final_van_position IS NOT NULL AND vote_commitment_positions IS NOT NULL)),
    CHECK (confirmation_source != 'hash' OR
        (confirmed_transaction_hash IS NOT NULL AND candidate_transaction_hash = confirmed_transaction_hash)),
    CHECK (confirmation_source != 'tree' OR confirmed_transaction_hash IS NULL)
);

INSERT INTO chain_submissions SELECT * FROM chain_submissions_v18;
DROP TABLE chain_submissions_v18;

CREATE UNIQUE INDEX chain_submissions_identity
    ON chain_submissions(wallet_id, network, round_id, kind, bundle_index,
                         ifnull(proposal_id, -1), ifnull(hex(ordered_batch_digest), ''));
CREATE UNIQUE INDEX chain_submissions_candidate_owner
    ON chain_submissions(candidate_transaction_hash)
    WHERE candidate_transaction_hash IS NOT NULL;
CREATE UNIQUE INDEX chain_submissions_confirmation_hash_owner
    ON chain_submissions(confirmed_transaction_hash)
    WHERE confirmed_transaction_hash IS NOT NULL;

CREATE TRIGGER chain_submissions_immutable_identity
BEFORE UPDATE OF identity_key, round_id, wallet_id, network,
                 bundle_index, kind, proposal_id, ordered_batch_digest,
                 generation_digest, created_at ON chain_submissions
BEGIN
    SELECT RAISE(ABORT, 'chain submission identity and generation are immutable');
END;
CREATE TRIGGER chain_submissions_monotonic_reservations
BEFORE UPDATE OF committed_post_reservations ON chain_submissions
WHEN NEW.committed_post_reservations < OLD.committed_post_reservations
BEGIN
    SELECT RAISE(ABORT, 'chain submission reservation count cannot decrease');
END;
CREATE TRIGGER chain_submissions_immutable_tracking_start
BEFORE UPDATE OF tracking_started_at ON chain_submissions
WHEN OLD.tracking_started_at IS NOT NULL AND NEW.tracking_started_at IS NOT OLD.tracking_started_at
BEGIN
    SELECT RAISE(ABORT, 'chain submission tracking start is immutable');
END;
