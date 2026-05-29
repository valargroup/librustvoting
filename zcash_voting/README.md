# zcash_voting

Client-side library for integrating [Zcash shielded voting](https://github.com/valargroup/vote-sdk) into a wallet. Wraps the Halo 2 ZKPs, hotkey derivation, share construction, and governance-PCZT assembly that a wallet needs to participate in an on-chain voting round.

## Usage

Wallets should import `zcash_voting::prelude::*` and follow the stable setup →
precompute → delegate → vote → share lifecycle:

1. Open a `VotingDb`, set the wallet id, and call `create_round`.
2. Convert eligible Orchard notes into `NoteInfo` with
   `NoteInfo::from_orchard_note`, then call `ensure_bundles`.
   The default `BundlePolicy` fills each bundle up to the circuit note-slot
   count. Wallets that need fewer real notes per bundle can call the
   `*_with_policy` variants with `BundlePolicy::new(...)`; proof construction
   still pads each bundle to the same fixed circuit slot count.
3. Build the governance PCZT with `setup_delegation`.
4. Precompute delegation inputs with `note_witnesses` and, with the `pir`
   feature, `delegation_pir`.
5. Prove with `delegate::prove`, assemble submission fields with
   `delegation_submission`, submit them through the wallet's chain client, and
   use `record_submission` while polling plus `confirm_delegation_submission`
   after confirmation.
6. Record each terminal ballot decision with `set_ballot_intent`, then use
   `vote::commit` to commit votes locally and submit cast-vote transactions.
   Submit helper shares after the cast-vote transaction is confirmed.
7. After restart, call `resume_plan` with the round's full proposal id list and
   execute one returned `NextStep`, persist its result, then call `resume_plan`
   again. `CastVote` includes the recorded choice, and `SubmitVote` resumes an
   already committed vote through `vote::submission`. For `SubmitVote`, submit
   those recovered cast-vote fields, persist the cast-vote tx hash with
   `vote::record_submission` while polling, then record confirmed cast-vote
   events with `confirm_vote_submission`. After confirmation, call
   `vote::recover_commit` again and use its helper-share payloads so they carry
   the confirmed VC position, then record each accepted helper share with
   `share::record`. `Decision::Skipped` is terminal, so `open_proposals`
   contains only proposals that have no recorded decision.

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
| `round` | `VotingDb`, `RoundParams`, `RoundInfo`, idempotent `ensure_bundles`, and policy-aware bundle planning. |
| `precompute` | Orchard note witness generation and PIR precompute wrappers. |
| `delegate` | PCZT setup, proof generation, submission assembly, and chain recovery writes. |
| `confirmation` | Chain tx event parsing plus atomic delegation and cast-vote confirmation recording. |
| `vote` | ZKP2 construction, cast-vote signing, and vote recovery bundle persistence. |
| `share` | Helper-share payload recovery, nullifier computation, and share confirmation state. |
| `session` | Durable ballot intent plus the round-level resume planner. |
| `phases` | Per-bundle `DelegationPhase` derived from persisted artifacts. |
| `pir` | PIR endpoint selection helpers and client re-exports. |
| `hotkey` | Canonical contextual and random voting hotkey derivation. |
| `governance` | Low-level governance derivations, `BALLOT_DIVISOR`, and the circuit note-slot count. |

Wallet integrations should use the lifecycle modules above instead of writing
storage rows directly.

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
- Use `BundlePolicy` plus the `*_with_policy` APIs when an integration needs
  fewer real notes per bundle. Omit the policy for the default circuit-slot
  behavior.
- Use `precompute::note_witnesses` instead of hand-validating cached
  `TreeState` bytes and manually constructing `WitnessData`.
- Use `delegate::submission` with `DelegationSigner::seed(seed, keys)` or
  `DelegationSigner::Keystone` instead of separate submission methods.
- Use `derive_voting_hotkey` for software wallets, `generate_random_voting_hotkey`
  for hardware wallets, and `voting_hotkey_from_seed` when reconstructing stored
  hardware hotkey seed material. The crate owns contextual mixing and raw Orchard
  delegation-address derivation.
- Use `confirmation::{confirm_delegation_submission, confirm_vote_submission}`
  after chain clients report confirmed delegation or cast-vote tx events. The
  confirmation API parses the chain `leaf_index` events and records tx hashes,
  VAN positions, and VC positions atomically.
- Use `session::resume_plan` instead of reconstructing what comes next from raw
  delegation, vote, and share phases in wallet code. Fetch step execution
  material through crate APIs such as `vote::submission`,
  `vote::recover_commit`, `share::*`, and the tx hash accessors.
- Use `vote::commit`, `vote::submission`, `vote::recover_commit`,
  `vote::record_submission`, and `vote::record_vc_position` for the cast-vote
  lifecycle. Wallets should not write recovery JSON, submission flags, or vote
  commitment positions directly.
- Pre-launch database migrations reset older schema versions; export local test
  state before opening an older wallet DB with this crate version.

## License

Dual-licensed under MIT or Apache-2.0. See [LICENSE-MIT](../LICENSE-MIT) and [LICENSE-APACHE](../LICENSE-APACHE).
