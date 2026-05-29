# zcash_voting

Client-side cryptographic library for Zcash shielded voting. Implements proof generation, vote construction, and tree synchronization for the [Zally governance protocol](https://github.com/valargroup/shielded-vote-book).

## Workspace Crates

| Crate | Description |
|-------|-------------|
| **zcash_voting** | Core library: ZKP delegation and vote proofs (Halo2), El Gamal encryption, governance PCZT construction, Merkle witness generation, SQLite round-state persistence |
| **vote-commitment-tree** | Append-only Poseidon Merkle tree for Vote Authority Notes and Vote Commitments |
| **vote-commitment-tree-client** | HTTP client and CLI for syncing the vote commitment tree from a chain node |

## Architecture

```
zcash_voting
├── vote-commitment-tree ──── imt-tree (vote-nullifier-pir)
├── vote-commitment-tree-client
├── pir-client (vote-nullifier-pir)
├── voting-circuits ── ZK delegation + vote proofs, orchard fork
└── librustzcash ───── pczt, zcash_keys, zcash_client_sqlite, ...
```

## Building

```bash
cargo check                    # check all crates
cargo build -p zcash_voting   # build just the core library
```

## Wallet API Lifecycle

New wallet integrations should import `zcash_voting::prelude::*` and use the
stage-oriented API:

- `round::*` creates rounds and binds eligible notes into bundles.
- `precompute::*` prepares Orchard witnesses, delegation PIR inputs, and VAN
  witnesses for vote proofs.
- `delegate::*` builds delegation PCZTs, proves delegation, and records VAN
  positions.
- `vote::*` builds ZKP #2, signs cast-vote payloads, persists the canonical
  `VoteRecoveryBundle`, and reconstructs vote-chain submissions after a crash.
- `share::*` recovers helper-share payloads, computes share nullifiers, applies
  share scheduling policy, and records helper-share confirmation state.
- `session::*` records durable ballot intent and returns a round-level
  `RoundPlan` with ordered `NextStep`s for restart recovery. Wallets should
  write `Decision::Choice` before starting a cast-vote flow, write
  `Decision::Skipped` for proposals the user intentionally leaves blank, and
  use `resume_plan` after restart to decide whether to delegate, poll
  delegation/vote transactions, cast remaining votes, or confirm helper shares.
  `CastVote` steps include the recorded choice. `SubmitVote` steps mean a vote
  was already committed locally and should be reconstructed with
  `vote::recover_commit` rather than rebuilt from a draft. Submit the recovered
  cast-vote fields and helper-share payloads, persist the cast-vote tx hash
  with `vote::record_submission`, then re-run the planner because later work
  may depend on on-chain confirmations. `open_proposals` contains only
  proposals with no terminal decision yet.

## Migrating 0.11 to 0.12

- Replace `VotingDb::build_vote_commitment` + `vote_commitment::sign_cast_vote`
  + `VotingDb::build_share_payloads` orchestration with `vote::commit`.
- Replace custom cast-vote recovery JSON with `vote::serialize_recovery` and
  `vote::parse_recovery`.
- Replace direct `VoteTreeSync` ownership with
  `precompute::{sync_vote_tree, van_witness, reset_vote_tree}`.
- Replace direct `share_tracking` calls with `share::*`, and `share_policy`
  imports with `share::policy::*`.
- Replace raw vote/share workflow SQL with
  `VotingDb::{vote_phase, vote_phases, share_phase, share_phases}`.
- Replace wallet-local recovery fusion with `session::resume_plan`; keep only
  wallet-specific networking, proof execution, signing, and UI routing at the
  wallet boundary.
- Do not use the legacy `VotingDb::store_commitment_bundle` writer for new
  integrations, or `VotingDb::mark_vote_submitted` as the submission marker.
  They are compatibility APIs. Use `vote::commit`, `vote::recover_commit`,
  `vote::record_submission`, and `vote::record_vc_position`.

Pre-launch wallet databases with older schema versions are reset when opened by
this branch; callers that need to preserve test data should export it before
upgrading the crate.

The workspace depends on the private [valargroup/voting-circuits](https://github.com/valargroup/voting-circuits) repo. The `.cargo/config.toml` enables `git-fetch-with-cli` so your local git credentials are used automatically.

## Dependency Strategy

This workspace uses `[patch.crates-io]` (in the root `Cargo.toml`) to override two dependency trees:

- **orchard 0.11** — Resolved from [valargroup/voting-circuits](https://github.com/valargroup/voting-circuits), which bundles an orchard fork with public visibility for `constants`, `spec`, and a `shared_primitives::spend_authority` gadget.

- **librustzcash crates** (pczt, zcash_keys, zcash_client_sqlite, etc.) — Resolved from [valargroup/librustzcash](https://github.com/valargroup/librustzcash) branch `valargroup/pczt-governance-extensions-0.11`. Adds public getters and methods needed for governance PCZT construction and Merkle witness generation.

## FFI

Mobile FFI bindings live in [zcash-swift-wallet-sdk](https://github.com/valargroup/zcash-swift-wallet-sdk) (hand-rolled C FFI + Swift wrappers). This repo is a pure Rust workspace.

## License

TODO
