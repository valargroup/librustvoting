# Changelog
All notable changes to this workspace will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this workspace adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

# Unreleased

## V2 API

### Added
- Added `session::resume_plan` plus a durable `ballot_intent` table (schema v11):
  a pure, I/O-free round-level planner that fuses the per-bundle delegation,
  vote, and share phases with the voter's recorded ballot intent into an ordered
  list of `NextStep`s, so wallet SDKs can resume an interrupted multi-question
  vote without re-deriving recovery state. Exported via the prelude
  (`Decision`, `NextStep`, `RoundPlan`, `resume_plan`). `NextStep` is
  `non_exhaustive`; `CastVote` carries the recorded choice, committed but
  unsubmitted votes resume through `SubmitVote`, and confirmed votes missing
  helper-share rows resume through per-share `SubmitShares` steps derived from
  recovered share payloads. Vote work is ordered by proposal before bundle so
  interrupted multi-bundle questions finish before later questions resume.
  Skipped ballot intents are terminal decisions, `open_proposals` contains only
  proposals with no recorded decision, and choice intents fail fast if no
  eligible bundle rows exist for the round. Intent changes that conflict with an
  already-submitted vote fail before any recovery rows are cleaned up, and stale
  vote submissions are rejected after an intent changes.
- Added `vote::recover_commit` for `NextStep::SubmitVote` handling. It
  reconstructs both cast-vote submission fields and helper-share payloads from
  persisted recovery state so wallets do not need to reassemble recovery JSON
  and share material manually, while `share::record` persists accepted recovered
  helper shares with crate-derived nullifiers.
- Added shared delegation request/report types, account-key loading, Keystone
  PCZT redaction, display memo formatting, prepared-PCZT caching, skipped-suffix
  bundle validation, and bundle weight helpers so wallet SDKs can keep only
  their runtime-specific async/lightwalletd shims.
- Added shared `lwd` helpers for mainnet lightwalletd channel setup, bounded
  unary RPCs, chain-tip lookup, consensus branch resolution, and snapshot
  `TreeState` fetching with retry so wallet SDKs no longer need local copies of
  these queries.
- Added shared wallet note-selection helpers and delegation input gathering
  (`select_snapshot_notes`, `select_snapshot_note_infos`, and
  `gather_delegation_wallet_inputs`) so wallet SDKs can reuse the snapshot
  eligibility, Orchard note-info extraction, and selected-note summary logic.
- Added `select_notes_with_wallet_db` and tree-sync-gated `select_notes_with_lwd`
  so wallet SDKs can reuse scan-height validation, wallet/network consistency
  checks, lightwalletd snapshot-anchor fetching, and selected-note assembly
  without carrying SDK-local wrapper logic.
- Added `BundlePolicy`, policy-aware note planning, and policy-aware delegation
  precompute entry points so wallet SDKs can choose how many real notes are
  placed in each bundle while the default fills each bundle up to the circuit
  note-slot count.
- Added library-owned delegation lifecycle stage reporting and branch-id
  provider traits so wallet SDKs can pass progress and consensus-branch
  resolution into `delegate::setup` and `delegate::prove` without duplicating
  library internals.
- Added crate-owned voting hotkey derivation with contextual software hotkeys,
  random hardware hotkeys, raw Orchard delegation-address derivation, and typed
  `DelegationKeys` / `VoteSigner` helpers so wallet SDKs no longer need to
  assemble hotkey seed material or pass raw hotkey address bytes by hand.
  `derive_voting_hotkey`, `generate_random_voting_hotkey`, and
  `voting_hotkey_from_seed` replace the older raw hotkey generation helpers
  exposed through `hotkey::generate_hotkey` and `VotingDb::generate_hotkey`.
- Added `delegate::LightwalletdBranchIdProvider` and
  `delegate::branch_id_for_height` so wallet SDKs can resolve delegation
  consensus branches from `lightwalletd_url` plus `Network` without duplicating
  lightwalletd tip-fetching code.
- Added `vote::VoteCommitStage` plus `VoteCommitStageReporter` and
  `VoteCommitStageBridge` so wallet SDKs can consume library-owned cast-vote
  lifecycle and proof-progress stages without defining local event enums.
- Added `VotingDb::prepare_delegation_pir` so wallet SDKs can share the
  delegation bundle validation, governance PCZT construction, and PIR precompute
  sequence while still supplying wallet-specific notes, account metadata, typed
  voting hotkey, consensus branch, and PIR transport at their own boundaries.
  Callers that need a non-default bundle policy can use
  `VotingDb::prepare_delegation_pir_with_policy`.
- Added `zcash_voting::witness::generate_note_witnesses` for Orchard note
  witness generation from a stored voting round snapshot. The V2 API validates
  the cached lightwalletd `TreeState` height and Orchard root against the
  persisted round parameters before asking the wallet DB for historical Merkle
  paths, then returns `WitnessData` for each bundled note.
- Added `zcash_voting::witness::store_tree_state_and_generate_note_witnesses`
  so wallet SDKs can share the snapshot tree-state persistence, witness
  generation, and bundle witness caching flow while keeping wallet DB opening at
  each SDK boundary.
- Added the stable `vote::*` cast-vote API with `DraftVote`, `VanWitness`,
  `VoteCommit`, `VoteSigner`, `VoteSubmission`, and `VoteRecoveryBundle`.
  `vote::commit` now builds ZKP #2, signs the cast-vote payload, persists the
  canonical recovery bundle, and can reconstruct submission fields after a
  process restart.
- Added the stable `share::*` API for helper-share nullifier computation,
  recovery payload reconstruction, share tracking persistence, confirmation,
  sent-server updates, and `share::policy::*` scheduling re-exports.
- Added `VotePhase` and `SharePhase` plus
  `VotingDb::{vote_phase, vote_phases, share_phase, share_phases}` so wallets
  can derive vote/share recovery state without querying SQLite tables directly.
- Added `precompute::{sync_vote_tree, van_witness, reset_vote_tree}` as the
  public vote commitment tree sync and VAN witness surface.
- Added `examples/end_to_end_vote.rs` and README notes for moving from the
  delegation-oriented V2 API to the new vote/share API.

### Changed
- Delegation PIR warmup no longer constructs or caches a governance PCZT.
  `precompute::precompute_delegation` now warms witnesses, padded-note secrets,
  and PIR rows only; `delegate::setup` builds the PCZT later from the persisted
  padded secrets and refuses to overwrite existing padded secrets or
  `pczt_sighash`.
- `DelegationKeys::with_hotkey_bytes` no longer accepts `consensus_branch_id`;
  `delegate::setup` now resolves it through a caller-supplied
  `BranchIdProvider`. Delegation proof progress is reported via
  `DelegationStageReporter`, while generic vote proof progress uses
  `ProgressReporter`.
- Vote recovery state is now guarded by durable vote identity. Stale recovery
  JSON, helper-share rows, tx hashes, and vote commitment tree positions cannot
  be attached to a replacement vote after the voter changes intent.
- Helper-share recording now rejects conflicting nullifiers for an existing
  share key in the shared storage layer.
- The raw nullifier-taking helper-share storage writer is now crate-internal.
  Wallet integrations use `share::record`, which derives the nullifier from
  persisted vote recovery state.
- Removed the legacy `VotingDb::mark_vote_submitted`,
  `VotingDb::store_vote_tx_hash`, and `VotingDb::store_commitment_bundle`
  writers, and dropped the stale `votes.submitted` column. Integrations now use
  `vote::commit`, `vote::recover_commit`, `vote::record_submission`, and
  `vote::record_vc_position`.
- `precompute::sync_vote_tree` now rebuilds a round's sparse vote-tree client
  when recovery records a new historical VAN position after an earlier sync,
  so wallets can resume interrupted multi-question votes without manually
  resetting tree state.
- Removed the old `note_bundling` JSON facade and duplicate note-plan schema.
  Smart bundle planning now lives in the slim `note_bundling` module and is
  exposed through the policy-aware `round` APIs. Lower-level public bundle setup
  helpers were removed in favor of `round` module APIs.
- `vote::serialize_recovery` / `vote::parse_recovery` now own the canonical
  `zcash_voting_vote_recovery_v1` recovery JSON format, replacing wallet-owned
  cast-vote recovery blobs.
- `tree_sync::VanWitness` now uses the typed `vote::VanWitness` shape with a
  fixed 24-element authentication path.
- `VotingHotkey` now represents the actual hotkey secret seed plus raw Orchard
  address. The old placeholder Pallas public key and `sv1...` address fields
  were removed.
- `VoteSigner::Seed` was renamed to `VoteSigner::HotkeySeed` to make the seed
  source explicit, and `vote_commitment::sign_cast_vote_for_account` was removed
  in favor of the canonical voting hotkey account index.
- Raw-byte `DelegationKeys` construction is no longer public. Wallet callers use
  `DelegationKeys::with_voting_hotkey`, and the crate derives network-specific
  metadata from the `VotingHotkey`.
- Low-level ZKP2 and cast-vote signing helpers that take raw hotkey seed plus
  `network_id` are now crate-internal. Wallet callers should use `vote::commit`
  with `VoteSigner`.

# 0.10.1

## Security
- Exact-pinned the Valar-owned voting dependency surface and related PIR/tree
  transitives used by the client features. `zcash_voting` now directly
  constrains `pir-client`, `pir-types`, `valar-spiral-rs`, `valar-ypir`,
  `imt-tree`, `voting-circuits`, `vote-commitment-tree`, and
  `vote-commitment-tree-client`.
- Bumped `vote-commitment-tree` to `0.3.1` and
  `vote-commitment-tree-client` to `0.5.1` for publishable manifest-only pin
  releases.

## Notes
- This is a supply-chain pin tightening release with no functional code
  changes.
- Scope is intentionally limited to the Valar-owned runtime voting dependency
  surface and its PIR/tree transitives. Upstream and dev-only dependency
  movement should be handled through lockfile review/CI policy rather than this
  manifest-only pinning release.

# 0.10.0

## Changed
- Bumped `voting-circuits` to `0.6.0` and removed the workspace patch override,
  so the SDK uses the published circuit crate for delegation proof generation.
- Updated wallet-side governance derivations to call the circuit crate's
  canonical helpers for nullifier domains, governance nullifiers, VAN
  commitments, and rho bindings. This is a breaking cryptographic derivation
  change for delegation proof compatibility.

# 0.9.2

## Fixed
- Matched wallet-side padded note commitments and nullifiers to the synthetic
  padding points introduced by `voting-circuits 0.5.0`, so delegation PIR
  precompute fetches the same padded IMT proofs that proof generation later
  requests.

# 0.9.1

## Added
- Added pure `share_policy`, `pir_snapshot`, and `note_bundling` APIs so wallet
  SDKs can share helper-share timing, exact PIR snapshot selection, and note
  bundle planning logic instead of reimplementing it in each app.

# 0.9.0

## Changed
- Bumped `voting-circuits` to `0.5.0` and updated callers to use its public
  re-exports and upstream circuit key caches.
- Bumped `vote-commitment-tree` to `0.3.0` and
  `vote-commitment-tree-client` to `0.5.0`.
- Removed local wallet-side test/helpers that duplicated vote-commitment and
  El Gamal internals now owned by `voting-circuits`.

# 0.8.1

## Fixed
- Recovery store operations now fail when their target bundle or vote row is
  missing instead of treating a zero-row SQLite update as success.

# 0.8.0

## Changed
- Reset the pre-launch SQLite schema history. Voting databases from interim
  schema versions are now recreated from the current `001_init.sql` baseline
  and marked as schema version 9.

# 0.7.1

## Added
- Added `NoteInfo::from_orchard_note` so SDK FFI layers can reuse the crate's
  Orchard note conversion logic instead of reconstructing `NoteInfo` fields
  themselves.

# 0.7.0

## Changed
- Removed the unused `round_id` parameter from `VotingDb::generate_hotkey`.

## Fixed
- Share payload construction now errors when the requested share is missing its
  blind instead of using empty bytes.
- Recovery now rejects stored commitment bundles that are missing their vote
  commitment tree position instead of assuming position 0.
- Delegation proof generation now requires the randomness saved when the PCZT was
  built instead of sampling fresh randomness when those fields are empty.

# 0.6.0

## Changed
- Bumped `zcash_voting` to `0.6.0`, `vote-commitment-tree` to `0.2.0`, and
  `vote-commitment-tree-client` to `0.4.0` for the breaking commitment leaf
  pagination API.
- Vote commitment tree sync now consumes paginated commitment leaf responses
  with per-block roots instead of issuing one request per height window.

# 0.5.12

## Fixed
- `zcash_voting::action::build_governance_pczt` now guarantees the returned
  `GovernancePczt` describes a single Orchard action: the spend producing
  `nf_signed`, `rk`, and `alpha` is the same action whose output produces
  `cmx_new` and `rseed_output`. The Orchard PCZT builder pads to two actions
  and shuffles spends and outputs independently, so previous calls could
  return metadata mixing two different randomized actions, which later caused
  `build_and_prove_delegation` to fail with `delegation proof result cmx_new
  does not match stored PCZT data`. The construction tail now retries
  `Builder::build_for_pczt` until `spend_idx == output_idx`, fails before
  persistence if no paired layout appears, and re-validates the serialized
  PCZT against the returned `action_index`.

# 0.5.10

## Changed
- Bumped `zcash_voting` to `0.5.10` and updated `voting-circuits` to `0.4.2`.

# 0.5.9

## Added
- Added `VotingDb::has_round` for checking round existence through the storage
  API without downstream callers depending on SQLite schema details.

# 0.5.8

## Added
- `VotingDb::setup_bundles` now persists bundle note identity hashes, and
  `VotingDb::build_governance_pczt`, `VotingDb::precompute_delegation_pir`,
  and `VotingDb::build_and_prove_delegation` reject same-position note
  substitutions for bundles set up under 0.5.8 or later. Bundles persisted by
  earlier releases retain the prior position-only check until they are
  re-setup.

## Fixed
- Delegation proof storage now checks proof-derived public inputs against the
  PCZT-derived values stored during `VotingDb::build_governance_pczt`, and
  stores the proof, public inputs, and round phase atomically.
- `VotingDb::setup_bundles` now persists all bundle rows in a single
  transaction.
- Avoided dropping the Hyper/Tokio transport runtime from inside an active Tokio
  context.

# 0.5.7

## Fixed
- `VotingDb::mark_vote_submitted` now returns an error when no persisted vote
  row matches the requested round, wallet, bundle, and proposal instead of
  treating a zero-row update as success.

# 0.5.6

## Added
- Added a `test-fixtures` feature exposing `VotingDb::insert_vote_fixture`, so
  downstream FFI tests can create vote rows through `VotingDb` instead of
  depending on SQLite schema internals.

# 0.5.5

## Fixed
- Keystone delegation submissions now reject a supplied sighash unless it matches
  the PCZT sighash stored for the bundle.

# 0.5.4

## Fixed
- Delegation submission signing now derives the sender spending key from the
  caller's ZIP-32 `account_index` instead of always using account 0.

# 0.5.3

## Fixed
- **`zcash_voting` `network_id` convention** now matches the wallet SDK everywhere
  (`zkp1::build_and_prove_delegation`, PIR `precompute_delegation_pir` padded
  nullifiers, `zkp2::derive_spending_key`, `vote_commitment::sign_cast_vote`, and
  storage helpers that take `network_id`): **0 = testnet, 1 = mainnet**. The
  padded-nullifier path had previously used the inverse mapping, so `NoteInfo`
  from the SDK could disagree with PIR precompute vs proof generation.

## Changed
- Bumped the `zcash_voting` crate version to `0.5.3`. Direct callers who flipped
  `network_id` to compensate for the old bug should pass the SDK value unchanged
  after upgrading.

# 0.5.2

## Changed
- Reissued the tree-sync transport release from the merged `main` history.
- Confirmed the Hyper/Rustls tree-sync transport against production vote-chain
  endpoints for non-empty rounds.

# 0.5.1

## Changed
- Moved vote commitment tree sync onto the injected transport boundary and
  provided a direct Hyper/Rustls transport from `zcash_voting`.
- Removed `reqwest` from `vote-commitment-tree-client`'s library path.

# 0.5.0

## Changed
- Made `client-pir` transport-agnostic. `zcash_voting` no longer pulls
  `reqwest`; callers must provide a `pir_client::Transport`.
- Added transport-aware PIR precompute/proving entry points so SDKs can provide
  their own HTTP stack.
- Consolidated PIR proof validation and client transport under the single
  `client-pir` feature.
- Added a direct Hyper/Rustls PIR transport under `client-pir` for consumers
  that do not provide their own transport.

# 0.4.1

## Added
- Split the `zcash_voting` network-facing `client` feature into granular
  `client-pir` and `client-tree-sync` features. The existing `client` feature
  remains as a backwards-compatible aggregate of both.
- Made the PIR proof conversion/validation helper available to downstream
  consumers so SDK FFI layers can validate PIR `ImtProofData` without
  enabling vote-commitment-tree sync.

## Changed
- Bumped the `zcash_voting` crate version to `0.4.1` for the additive feature
  split.
