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
    delegation_pczt     BLOB,
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
    kind                         TEXT NOT NULL CHECK (kind IN ('delegation','vote','vote_batch','delegate_and_cast_vote_batch')),
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
        OR (kind IN ('vote_batch','delegate_and_cast_vote_batch') AND proposal_id IS NULL AND length(ordered_batch_digest) = 32)),
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

-- Round-wide immediate helper share designation. One row per wallet and
-- round names the share submitted immediately; it is written once, in the
-- same transaction as the designated vote's helper plan, and never updated.
-- It is voided with the undispatched generation it was made for, on the same
-- condition that clears the vote's helper plan.
CREATE TABLE round_immediate_share (
    round_id      TEXT NOT NULL,
    wallet_id     TEXT NOT NULL DEFAULT '',
    bundle_index  INTEGER NOT NULL CHECK (bundle_index BETWEEN 0 AND 4294967295),
    proposal_id   INTEGER NOT NULL CHECK (proposal_id BETWEEN 1 AND 50),
    share_index   INTEGER NOT NULL CHECK (share_index >= 0),
    designated_at INTEGER NOT NULL CHECK (designated_at >= 0),
    PRIMARY KEY (round_id, wallet_id),
    FOREIGN KEY (round_id, wallet_id, bundle_index, proposal_id)
        REFERENCES votes(round_id, wallet_id, bundle_index, proposal_id) ON DELETE CASCADE
);

CREATE TRIGGER round_immediate_share_immutable
BEFORE UPDATE ON round_immediate_share
BEGIN
    SELECT RAISE(ABORT, 'round immediate share designation is immutable');
END;

CREATE TRIGGER clear_round_immediate_share_on_vote_generation_change
AFTER UPDATE OF commitment_bundle_json ON votes
WHEN OLD.commitment_bundle_json IS NOT NEW.commitment_bundle_json
 AND NOT (
        OLD.vc_tree_position IS NULL
        AND NEW.vc_tree_position IS NOT NULL
        AND json_set(
                OLD.commitment_bundle_json,
                '$.vc_tree_position',
                NEW.vc_tree_position
            ) = NEW.commitment_bundle_json
    )
BEGIN
    DELETE FROM round_immediate_share
     WHERE round_id = NEW.round_id AND wallet_id = NEW.wallet_id
       AND bundle_index = NEW.bundle_index AND proposal_id = NEW.proposal_id;
END;

-- Immutable public delegation authorization; private setup remains in bundles/proofs.
CREATE TABLE delegate_cast_recovery (
    round_id TEXT NOT NULL,
    wallet_id TEXT NOT NULL,
    bundle_index INTEGER NOT NULL,
    batch_digest BLOB NOT NULL CHECK(length(batch_digest) = 32),
    delegation_generation_digest BLOB NOT NULL CHECK(length(delegation_generation_digest) = 32),
    spend_auth_signature BLOB NOT NULL CHECK(length(spend_auth_signature) = 64),
    PRIMARY KEY(round_id, wallet_id, bundle_index, batch_digest),
    FOREIGN KEY(round_id, wallet_id) REFERENCES rounds(round_id, wallet_id) ON DELETE CASCADE
);
CREATE TRIGGER delegate_cast_recovery_immutable BEFORE UPDATE ON delegate_cast_recovery
BEGIN SELECT RAISE(ABORT, 'combined delegation authorization is immutable'); END;

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
