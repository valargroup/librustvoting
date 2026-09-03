CREATE TABLE rounds (
    round_id            TEXT NOT NULL,
    wallet_id           TEXT NOT NULL DEFAULT '',
    network             TEXT NOT NULL CHECK (network IN ('mainnet', 'testnet', 'regtest')),
    snapshot_height     INTEGER NOT NULL,
    ea_pk               BLOB NOT NULL,
    nc_root             BLOB NOT NULL,
    nullifier_imt_root  BLOB NOT NULL,
    session_json        TEXT,
    phase               INTEGER NOT NULL DEFAULT 0,
    created_at          INTEGER NOT NULL,
    bundle_policy_json  TEXT,
    PRIMARY KEY (round_id, wallet_id)
);

CREATE TABLE bundles (
    round_id            TEXT NOT NULL,
    wallet_id           TEXT NOT NULL DEFAULT '',
    bundle_index        INTEGER NOT NULL,
    note_positions_blob BLOB,
    note_identity_hashes_blob BLOB,
    van_comm_rand       BLOB,
    dummy_nullifiers    BLOB,
    rho_signed          BLOB,
    padded_note_data    BLOB,
    nf_signed           BLOB,
    cmx_new             BLOB,
    alpha               BLOB,
    rseed_signed        BLOB,
    rseed_output        BLOB,
    gov_comm            BLOB,
    total_note_value    INTEGER,
    address_index       INTEGER,
    van_leaf_position   INTEGER,
    rk                  BLOB,
    gov_nullifiers_blob BLOB,
    padded_note_secrets BLOB,
    pczt_sighash        BLOB,
    tx1_effects         BLOB,
    delegation_tx_hash  TEXT,
    PRIMARY KEY (round_id, wallet_id, bundle_index),
    FOREIGN KEY (round_id, wallet_id) REFERENCES rounds(round_id, wallet_id) ON DELETE CASCADE
);

CREATE TABLE cached_tree_state (
    round_id        TEXT NOT NULL,
    wallet_id       TEXT NOT NULL DEFAULT '',
    snapshot_height INTEGER NOT NULL,
    tree_state      BLOB NOT NULL,
    PRIMARY KEY (round_id, wallet_id),
    FOREIGN KEY (round_id, wallet_id) REFERENCES rounds(round_id, wallet_id) ON DELETE CASCADE
);

CREATE TABLE proofs (
    round_id      TEXT NOT NULL,
    wallet_id     TEXT NOT NULL DEFAULT '',
    bundle_index  INTEGER NOT NULL,
    witness       BLOB,
    proof         BLOB,
    success       INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    PRIMARY KEY (round_id, wallet_id, bundle_index),
    FOREIGN KEY (round_id, wallet_id, bundle_index) REFERENCES bundles(round_id, wallet_id, bundle_index) ON DELETE CASCADE
);

CREATE TABLE witnesses (
    round_id        TEXT NOT NULL,
    wallet_id       TEXT NOT NULL DEFAULT '',
    bundle_index    INTEGER NOT NULL,
    note_position   INTEGER NOT NULL,
    note_commitment BLOB NOT NULL,
    root            BLOB NOT NULL,
    auth_path       BLOB NOT NULL,
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (round_id, wallet_id, bundle_index, note_position),
    FOREIGN KEY (round_id, wallet_id, bundle_index) REFERENCES bundles(round_id, wallet_id, bundle_index) ON DELETE CASCADE
);

CREATE TABLE votes (
    id              INTEGER PRIMARY KEY,
    round_id        TEXT NOT NULL,
    wallet_id       TEXT NOT NULL DEFAULT '',
    bundle_index    INTEGER NOT NULL,
    proposal_id     INTEGER NOT NULL,
    choice          INTEGER NOT NULL,
    commitment      BLOB,
    created_at      INTEGER NOT NULL,
    tx_hash                 TEXT,
    vc_tree_position        INTEGER,
    commitment_bundle_json  TEXT,
    UNIQUE(round_id, wallet_id, bundle_index, proposal_id),
    FOREIGN KEY (round_id, wallet_id, bundle_index) REFERENCES bundles(round_id, wallet_id, bundle_index) ON DELETE CASCADE
);

CREATE TABLE helper_share_plans (
    round_id                    TEXT NOT NULL,
    wallet_id                   TEXT NOT NULL DEFAULT '',
    bundle_index                INTEGER NOT NULL,
    proposal_id                 INTEGER NOT NULL,
    commitment_bundle_json      TEXT NOT NULL,
    configured_server_urls_json TEXT NOT NULL,
    share_plans_json            TEXT NOT NULL,
    format_version              INTEGER NOT NULL CHECK (format_version = 1),
    placement_guarantee         TEXT NOT NULL CHECK (placement_guarantee IN ('strict','legacy_best_effort')),
    created_at                  INTEGER NOT NULL,
    PRIMARY KEY (round_id, wallet_id, bundle_index, proposal_id),
    FOREIGN KEY (round_id, wallet_id, bundle_index, proposal_id)
        REFERENCES votes(round_id, wallet_id, bundle_index, proposal_id) ON DELETE CASCADE
);

CREATE TRIGGER clear_helper_share_plan_on_vote_generation_change
AFTER UPDATE OF commitment_bundle_json ON votes
WHEN OLD.commitment_bundle_json IS NOT NEW.commitment_bundle_json
BEGIN
    -- Confirmation is the one non-generational recovery update: it fills the
    -- VC tree position in both the vote column and the otherwise-identical
    -- recovery JSON. Advance only a plan bound to the exact OLD snapshot and
    -- only when replacing that one JSON field produces the exact NEW bytes.
    UPDATE helper_share_plans
       SET commitment_bundle_json = NEW.commitment_bundle_json
     WHERE round_id = NEW.round_id AND wallet_id = NEW.wallet_id
       AND bundle_index = NEW.bundle_index AND proposal_id = NEW.proposal_id
       AND commitment_bundle_json = OLD.commitment_bundle_json
       AND OLD.vc_tree_position IS NULL
       AND NEW.vc_tree_position IS NOT NULL
       AND json_set(
               OLD.commitment_bundle_json,
               '$.vc_tree_position',
               NEW.vc_tree_position
           ) = NEW.commitment_bundle_json;
    DELETE FROM helper_share_plans
     WHERE round_id = NEW.round_id AND wallet_id = NEW.wallet_id
       AND bundle_index = NEW.bundle_index AND proposal_id = NEW.proposal_id
       AND commitment_bundle_json IS NOT NEW.commitment_bundle_json;
END;

CREATE TABLE share_delegations (
    round_id        TEXT NOT NULL,
    wallet_id       TEXT NOT NULL DEFAULT '',
    bundle_index    INTEGER NOT NULL,
    proposal_id     INTEGER NOT NULL,
    share_index     INTEGER NOT NULL,
    sent_to_urls    TEXT NOT NULL,
    nullifier       BLOB NOT NULL,
    confirmed       INTEGER NOT NULL DEFAULT 0,
    submit_at       INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    ambiguous_urls  TEXT NOT NULL DEFAULT '[]',
    attempting_urls TEXT NOT NULL DEFAULT '[]',
    target_count    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (round_id, wallet_id, bundle_index, proposal_id, share_index),
    FOREIGN KEY (round_id, wallet_id, bundle_index)
        REFERENCES bundles(round_id, wallet_id, bundle_index) ON DELETE CASCADE
);

CREATE TABLE keystone_signatures (
    round_id        TEXT NOT NULL,
    wallet_id       TEXT NOT NULL DEFAULT '',
    bundle_index    INTEGER NOT NULL,
    sig             BLOB NOT NULL,
    sighash         BLOB NOT NULL,
    rk              BLOB NOT NULL,
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (round_id, wallet_id, bundle_index),
    FOREIGN KEY (round_id, wallet_id, bundle_index)
        REFERENCES bundles(round_id, wallet_id, bundle_index) ON DELETE CASCADE
);

CREATE TABLE ballot_intent (
    round_id     TEXT NOT NULL,
    wallet_id    TEXT NOT NULL DEFAULT '',
    proposal_id  INTEGER NOT NULL,
    skipped      INTEGER NOT NULL DEFAULT 0,  -- 1 = intentionally skipped
    choice       INTEGER,                     -- 0-indexed option; NULL iff skipped
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (round_id, wallet_id, proposal_id),
    FOREIGN KEY (round_id, wallet_id) REFERENCES rounds(round_id, wallet_id) ON DELETE CASCADE,
    CHECK ((skipped = 1 AND choice IS NULL) OR (skipped = 0 AND choice IS NOT NULL))
);

CREATE TABLE pir_proof_cache (
    wallet_id   TEXT NOT NULL DEFAULT '',
    network     TEXT NOT NULL CHECK (network IN ('mainnet','testnet','regtest')),
    nullifier   BLOB NOT NULL,
    root        BLOB NOT NULL,
    nf_bounds   BLOB NOT NULL,
    leaf_pos    INTEGER NOT NULL,
    path        BLOB NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (wallet_id, network, root, nullifier)
);

-- Authoritative SDK-owned vote-chain submission lifecycle. One identity per
-- row, created only by runtime reservation; no version-17 evidence is ever
-- imported here. The configured vote-chain id is dispatch routing and is
-- deliberately absent: it binds neither the identity nor the generation
-- digest. Every row carries the generation digest it was reserved for.
CREATE TABLE chain_submissions (
    identity_key                 BLOB NOT NULL PRIMARY KEY,
    round_id                     TEXT NOT NULL,
    wallet_id                    TEXT NOT NULL DEFAULT '',
    network                      TEXT NOT NULL CHECK (network IN ('mainnet','testnet','regtest')),
    bundle_index                 INTEGER NOT NULL CHECK (bundle_index BETWEEN 0 AND 4294967295),
    kind                         TEXT NOT NULL CHECK (kind IN ('delegation','vote','vote_batch')),
    proposal_id                  INTEGER,
    ordered_batch_digest         BLOB,
    generation_digest            BLOB NOT NULL CHECK (length(generation_digest) = 32),
    state                        TEXT NOT NULL CHECK (state IN ('submitting','tracking','recovering','submitted_without_hash','confirmed','rejected')),
    candidate_transaction_hash   BLOB,
    committed_post_reservations  INTEGER NOT NULL DEFAULT 0 CHECK (committed_post_reservations >= 0),
    tracking_started_at          INTEGER,
    diagnostic_kind              TEXT,
    diagnostic                   TEXT,
    confirmation_source          TEXT CHECK (confirmation_source IN ('hash','tree')),
    confirmed_transaction_hash   BLOB,
    final_van_position           INTEGER,
    vote_commitment_positions    BLOB,
    created_at                   INTEGER NOT NULL CHECK (created_at >= 0),
    updated_at                   INTEGER NOT NULL CHECK (updated_at >= created_at),
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
