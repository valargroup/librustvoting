# zcash_voting

Client-side library for integrating [Zcash shielded voting](https://github.com/valargroup/vote-sdk) into a wallet. Wraps the Halo 2 ZKPs, hotkey derivation, share construction, and governance-PCZT assembly that a wallet needs to participate in an on-chain voting round.

## Usage

Wallets should import `zcash_voting::prelude::*` and follow the stable setup →
precompute → delegate → vote → share lifecycle:

1. Open a `VotingDb`, set the wallet id, and call `create_round`.
2. Convert eligible Orchard notes into `NoteInfo` with
   `NoteInfo::from_orchard_note`, then call `ensure_bundles`.
3. Build the governance PCZT with `setup_delegation`.
4. Precompute delegation inputs with `note_witnesses` and, with the `pir`
   feature, `delegation_pir`.
5. Prove with `delegate::prove`, assemble submission fields with
   `delegation_submission`, and record chain recovery data with
   `record_submission` and `record_van_position`.
6. Record each terminal ballot decision with `set_ballot_intent`, then use
   `vote::commit` and `share::*` to submit votes and helper shares.
7. After restart, call `resume_plan` with the round's full proposal id list and
   execute one returned `NextStep`, persist its result, then call `resume_plan`
   again. `CastVote` includes the recorded choice, and `SubmitVote` resumes an
   already committed vote through `vote::recover_commit`. `Decision::Skipped`
   is terminal, so `open_proposals` contains only proposals that have no
   recorded decision.

## Crate layout

| Crate | Purpose |
|---|---|
| **`zcash_voting`** (this crate) | Stable wallet API: round setup, note bundles, delegation precompute/proving, hotkey derivation, and round-state storage. |
| [`vote-commitment-tree`](../vote-commitment-tree) | Append-only Poseidon Merkle tree for VANs and vote commitments. |
| [`vote-commitment-tree-client`](../vote-commitment-tree-client) | HTTP client + CLI for syncing the vote commitment tree from a running chain node. |

## Public modules

| Module | Purpose |
|---|---|
| `prelude` | Recommended imports for wallet SDKs. |
| `round` | `VotingDb`, `RoundParams`, `RoundInfo`, and idempotent `ensure_bundles`. |
| `precompute` | Orchard note witness generation and PIR precompute wrappers. |
| `delegate` | PCZT setup, proof generation, submission assembly, and chain recovery writes. |
| `vote` | ZKP2 construction, cast-vote signing, and vote recovery bundle persistence. |
| `share` | Helper-share payload recovery, nullifier computation, and share confirmation state. |
| `session` | Durable ballot intent plus the round-level resume planner. |
| `phases` | Per-bundle `DelegationPhase` derived from persisted artifacts. |
| `pir` | PIR endpoint selection helpers and client re-exports. |
| `hotkey` | Primitive hotkey derivation from caller-supplied seed bytes. |
| `governance` | Low-level governance derivations and `BALLOT_DIVISOR`. |

Lower-level modules from previous releases remain available during the 0.11
migration window, but new wallet code should prefer the lifecycle modules above.

## Shared wallet policy helpers

The `share_policy` module contains pure helpers for wallet-side voting behavior
that should stay consistent across SDKs:

- delayed helper-share `submit_at` scheduling
- helper target counts and randomized helper ordering
- batch share planning with independent entropy per share
- resubmission ordering with untried helpers before already-sent helpers
- share tracking summaries, readiness checks, retry thresholds, and polling delay

Wallet SDKs should provide fresh CSPRNG bytes from their platform RNG and let the
crate own the sampling and ordering policy.

## Dependency notes

`zcash_voting` tracks the upstream Zcash crates directly:

- **`orchard 0.13.1`** from crates.io, with the
  `unstable-voting-circuits` feature enabled for the governance proof paths.
- **`voting-circuits 0.6.0`** for the delegation and vote proof circuits.
- **`vote-commitment-tree 0.3`** and **`vote-commitment-tree-client 0.5`** for
  vote commitment tree state and optional HTTP sync.
- **`pczt`, `zcash_keys`, `zcash_primitives`, and `zcash_protocol`** from the
  published upstream Zcash crate line used by this release.

## Migrating from 0.10

- Enable `pir` and `tree-sync` instead of `client-pir` and
  `client-tree-sync`. The old feature names remain aliases for existing
  consumers during migration.
- Prefer `VotingDb::create_round`, `VotingDb::ensure_bundles`, and
  `VotingDb::delegation_phases` over direct `storage::queries` calls.
- Use `precompute::note_witnesses` instead of hand-validating cached
  `TreeState` bytes and manually constructing `WitnessData`.
- Use `delegate::submission` with `DelegationSigner::Seed` or
  `DelegationSigner::Keystone` instead of separate submission methods.
- Treat contextual hotkey mixing as wallet policy. The library intentionally
  keeps `generate_hotkey(seed)` primitive.
- Use `session::resume_plan` instead of reconstructing round recovery from raw
  delegation, vote, and share tables in wallet code.
- Do not use the legacy `VotingDb::store_commitment_bundle` writer for new
  integrations, or `VotingDb::mark_vote_submitted` as the submission marker.
  They are compatibility APIs. Use `vote::commit`, `vote::recover_commit`,
  `vote::record_submission`, and `vote::record_vc_position`.
- Pre-launch database migrations reset older schema versions; export local test
  state before opening an older wallet DB with this crate version.

## License

Dual-licensed under MIT or Apache-2.0. See [LICENSE-MIT](../LICENSE-MIT) and [LICENSE-APACHE](../LICENSE-APACHE).
