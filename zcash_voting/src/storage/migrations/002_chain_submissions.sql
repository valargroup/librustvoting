CREATE TABLE chain_submissions (
    identity_key BLOB NOT NULL PRIMARY KEY,
    round_id TEXT NOT NULL,
    wallet_id TEXT NOT NULL DEFAULT '',
    network TEXT NOT NULL CHECK (network IN ('mainnet','testnet','regtest')),
    vote_chain_id TEXT,
    bundle_index INTEGER NOT NULL CHECK (bundle_index BETWEEN 0 AND 4294967295),
    kind TEXT NOT NULL CHECK (kind IN ('delegation','vote','vote_batch')),
    proposal_id INTEGER,
    ordered_batch_digest BLOB,
    generation_digest BLOB,
    state TEXT NOT NULL CHECK (state IN ('submitting','tracking','recovering','confirmed','legacy_confirmed','rejected')),
    candidate_transaction_hash BLOB,
    committed_post_reservations INTEGER NOT NULL DEFAULT 0 CHECK (committed_post_reservations >= 0),
    tracking_started_at INTEGER,
    diagnostic_kind TEXT,
    diagnostic TEXT,
    confirmation_source TEXT CHECK (confirmation_source IN ('hash','tree','legacy_import','legacy_projection')),
    confirmed_transaction_hash BLOB,
    final_van_position INTEGER,
    vote_commitment_positions BLOB,
    created_at INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at INTEGER NOT NULL CHECK (updated_at >= created_at),
    FOREIGN KEY (round_id, wallet_id) REFERENCES rounds(round_id, wallet_id) ON DELETE CASCADE,
    CHECK (length(identity_key) >= 32),
    CHECK (vote_chain_id IS NULL OR length(vote_chain_id) BETWEEN 1 AND 128),
    CHECK ((kind = 'delegation' AND proposal_id IS NULL AND ordered_batch_digest IS NULL)
        OR (kind = 'vote' AND proposal_id BETWEEN 1 AND 15 AND ordered_batch_digest IS NULL)
        OR (kind = 'vote_batch' AND proposal_id IS NULL AND length(ordered_batch_digest) = 32)),
    CHECK ((vote_chain_id IS NULL AND generation_digest IS NULL
            AND state IN ('recovering','legacy_confirmed')
            AND candidate_transaction_hash IS NULL
            AND committed_post_reservations = 0 AND tracking_started_at IS NULL)
        OR (vote_chain_id IS NOT NULL AND length(generation_digest) = 32)),
    CHECK (candidate_transaction_hash IS NULL OR length(candidate_transaction_hash) = 32),
    CHECK (confirmed_transaction_hash IS NULL OR length(confirmed_transaction_hash) = 32),
    CHECK ((state = 'submitting' AND candidate_transaction_hash IS NULL AND tracking_started_at IS NULL)
        OR (state = 'tracking' AND candidate_transaction_hash IS NOT NULL AND tracking_started_at IS NOT NULL)
        OR state IN ('recovering','confirmed','legacy_confirmed','rejected')),
    CHECK ((diagnostic_kind IS NULL) = (diagnostic IS NULL)),
    CHECK (diagnostic IS NULL OR length(CAST(diagnostic AS BLOB)) <= 512),
    CHECK ((state IN ('confirmed','legacy_confirmed')) = (confirmation_source IS NOT NULL)),
    CHECK ((state NOT IN ('confirmed','legacy_confirmed'))
        OR (final_van_position IS NOT NULL AND vote_commitment_positions IS NOT NULL)),
    CHECK (state != 'legacy_confirmed' OR
        (kind = 'vote' AND vote_chain_id IS NULL AND confirmation_source = 'legacy_projection'
         AND confirmed_transaction_hash IS NULL)),
    CHECK (state != 'confirmed' OR confirmation_source != 'legacy_projection'),
    CHECK (confirmation_source != 'hash' OR
        (confirmed_transaction_hash IS NOT NULL AND candidate_transaction_hash = confirmed_transaction_hash)),
    CHECK (confirmation_source NOT IN ('tree','legacy_projection') OR confirmed_transaction_hash IS NULL)
);

CREATE UNIQUE INDEX chain_submissions_native_identity
    ON chain_submissions(wallet_id, network, vote_chain_id, round_id, kind, bundle_index,
                         ifnull(proposal_id, -1), ifnull(hex(ordered_batch_digest), ''))
    WHERE vote_chain_id IS NOT NULL;
CREATE UNIQUE INDEX chain_submissions_candidate_owner
    ON chain_submissions(candidate_transaction_hash)
    WHERE candidate_transaction_hash IS NOT NULL;
CREATE UNIQUE INDEX chain_submissions_confirmation_hash_owner
    ON chain_submissions(confirmed_transaction_hash)
    WHERE confirmed_transaction_hash IS NOT NULL;
CREATE UNIQUE INDEX chain_submissions_legacy_singleton_guard
    ON chain_submissions(wallet_id, network, round_id, bundle_index, proposal_id)
    WHERE vote_chain_id IS NULL AND kind = 'vote';

CREATE TRIGGER chain_submissions_immutable_identity
BEFORE UPDATE OF identity_key, round_id, wallet_id, network, vote_chain_id,
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
