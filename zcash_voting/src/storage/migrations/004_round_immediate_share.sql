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

-- Version-19 databases carried the designation only as an `immediate` marker
-- inside the designated vote's persisted plan. Adopt the first marked plan
-- per round; the designation is always domain share index 0.
INSERT OR IGNORE INTO round_immediate_share
    (round_id, wallet_id, bundle_index, proposal_id, share_index, designated_at)
SELECT p.round_id, p.wallet_id, p.bundle_index, p.proposal_id, 0, p.created_at
  FROM helper_share_plans p, json_each(p.share_plans_json) j
 WHERE json_extract(j.value, '$.immediate') = 1
 ORDER BY p.round_id, p.wallet_id, p.bundle_index, p.proposal_id;
