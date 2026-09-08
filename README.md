# zcash_voting

Client-side cryptographic library for Zcash shielded voting. Implements proof generation, vote construction, and tree synchronization for the [Zally governance protocol](https://github.com/valargroup/shielded-vote-book).

## Workspace Crates

| Crate | Description |
|-------|-------------|
| **zcash_voting** | Core library: ZKP delegation and vote proofs (Halo2), El Gamal encryption, governance PCZT construction, Merkle witness generation, chain confirmation parsing, SQLite round-state persistence |
| **vote-commitment-tree** | Append-only Poseidon Merkle tree for Vote Authority Notes and Vote Commitments |
| **vote-commitment-tree-client** | HTTP client and CLI for syncing the vote commitment tree from a chain node |

## Architecture

```
zcash_voting
├── config ───────────────────── config resolution + switch decisions
├── vote-commitment-tree-client ─ vote-commitment-tree
├── pir-client / vote-nullifier-pir types
├── voting-circuits ───────────── ZK delegation + vote proofs
└── librustzcash crates ───────── pczt, zcash_keys, zcash_client_sqlite, ...
```

The config resolver itself is transport-agnostic. Wallets choose the static
config source and network transport, fetch bytes, and pass those bytes into
`zcash_voting::config`. The `wallet-example::example_config` module shows a
direct HTTPS implementation for Rust consumers that do not need a custom
transport.

## Building

```bash
cargo check                  # check the default Zakura wallet path
cargo build -p zcash_voting  # build just the core library
```

Run the default Zakura tests with:

```bash
cargo test -p zcash_voting --features test-fixtures --locked
```

Run the LRZ Ironwood / NU6.3 tests with:

```bash
cargo test -p zcash_voting --no-default-features --features lrz --locked
```

The `zakura` and `lrz` features are mutually exclusive and pull separate crypto
stacks, so alternating between them in one target directory recompiles the whole
Halo2 dependency graph each time. Contributors who switch backends often can use
the `make` targets, which give each feature permutation its own
`CARGO_TARGET_DIR`; run `make help` for the list.

## Releases and Branching

`main` is the development line for the next release. Each shipped major series
is maintained on a `release/vMAJOR.x` branch, and semver-compatible fixes reach
those branches through reviewed automated backports rather than direct pushes.

See [Release branches and backports](docs/release-branches.md) for the backport
labels and the rules for what may ship on a maintenance line, and
[CONTRIBUTING.md](CONTRIBUTING.md) for the build, test, and code standards.

## Wallet API Lifecycle

New wallet integrations should import `zcash_voting::prelude::*` and drive a
round through `RoundDriver`: bind the round, its proposal roster, and the
voting hotkey secret to a `RoundExecutor` once, record decisions with
`set_ballot_intents`, then call `RoundDriver::run` and read the
`RoundRunReport` it returns. The driver owns the loop — it re-plans from
durable state, chooses what to run, overlaps independent bundles, paces a
still-tracking submission, and isolates a failure to its bundle — and stops
with a `RoundQuiescence` naming the state only a host can resolve: an open
ballot, delegation signatures it has not collected, a terminal submission, or
nothing left to do. Running the round's steps is the driver's: it carries the
epoch it dispatched under into each step, so a host switching session or
account interrupts work already in flight instead of having it adopted.
The executor owns the ordering between helper-plan persistence, chain
advancement, confirmation, and share delivery, proves off the async runtime,
and reports typed progress.
Supply transports once (`HyperTransport::with_route` over a host `RouteHttp`
for Tor or proxies), a `DelegationPipeline` for delegation steps, and a
`PirFleet` for PIR proofs. PIR and vote-tree traffic use whatever transports
the host binds; a host that wants them on a privacy route binds routed
transports for them too.

The stage-oriented modules below remain available for integrations that need
finer control:

- `round::*` creates rounds and binds eligible notes into bundles. Planning
  trims the low-value bundle tail so a concentrated holder emits fewer
  delegation submissions, bounded by the smaller of 1% of selected note value
  and 1,000 ZEC by default. `PrivacyTrim` reports the raw note value excluded,
  not bundle-quantized voting weight.
- `precompute::*` prepares shielded note witnesses, delegation PIR inputs, and VAN
  witnesses for vote proofs. `precompute_pir_proofs` warms PIR nullifier proofs in
  the background before any round or bundle exists (no hotkey needed), keyed by
  network and served IMT root; it plans the selected note set with the caller's
  `BundlePolicy` first so a dust tail is not fetched. The delegation prove path
  reads the same cache. `validate_cached_pir_proofs` checks warmed proofs against
  a round's `nullifier_imt_root` offline. Snapshots coexist in the cache;
  leftover roots are unused, and background warmup prunes cache rows created
  more than four weeks ago.
- `delegate::*` builds delegation PCZTs, proves delegation, prepares signing
  requests, and assembles signed delegation submissions. Wallets keep root seed
  material outside this crate, sign requests at the wallet boundary, and pass
  only signature bytes back through `PreparedSigner::signature`.
- `chain_submission::ChainSubmissionClient` is the only route to chain
  confirmation. Its `advance_*` calls parse delegation and cast-vote tx events
  internally and record tx hashes and tree positions atomically; hosts never
  handle chain events themselves.
- `vote::*` builds ZKP #2, signs cast-vote payloads, persists the canonical
  `VoteRecoveryBundle`, and reconstructs vote-chain submissions after a crash.
- `share::*` computes helper-share nullifiers and applies scheduling policy.
  `HelperClient::preflight_fleet`, `CommittedVote::prepare_share_delivery`, and
  `ConfirmedVote::submit_prepared_shares` own validated, journaled initial
  delivery, while `track_pending_shares` requires two distinct configured
  helpers to agree before persisting confirmation.
- `session::*` records durable ballot intent and returns a round-level
  `RoundPlan` with ordered `NextStep`s for restart recovery. Wallets should
  write `Decision::Choice` with the proposal's declared option count before
  starting a cast-vote flow, write `Decision::Skipped` with the same option
  count for proposals the user intentionally leaves blank, and use `resume_plan`
  after restart to decide whether to delegate, poll delegation/vote
  transactions, cast remaining votes, or confirm helper shares.
  `CastVote` steps include the recorded choice. `AdvanceVote` resumes one
  singleton through `ChainSubmissionClient::advance_vote_with_recovery` with
  `ChainRecoveryMode::ExactTree`, and
  `AdvanceVoteBatch` identifies the first ordered action as a recovery anchor
  for `ChainSubmissionClient::advance_vote_batch_with_recovery` in the same
  mode. `AdvanceDelegation` likewise uses
  `advance_delegation_with_recovery(..., ExactTree, ...)`. The lifecycle owns
  dispatch, polling, recovery, and confirmation, so submitting and polling are
  one step and one host call. Steps derive from the authoritative `chain_submissions`
  row, so a generation that is `Submitting`, `Tracking`, or `Recovering` yields
  an advance step and never a second submission. Read the plan's derived
  booleans (`needs_delegation_signing`, `has_in_flight_delegation`,
  `needs_vote_polling`, `has_remaining_vote_or_share_work`,
  `has_recoverable_vote_or_share_work`) rather than matching step kinds:
  they are computed from an exhaustive match, so a new step kind cannot
  silently read as "no work". Recover each
  `vote::CommittedVote`, validate and rank the complete helper fleet with
  `HelperClient::preflight_fleet`, then call
  `CommittedVote::prepare_share_delivery` with the complete proposal id roster
  from the authenticated round configuration. The SDK requires matching
  terminal ballot intents and derives the round's single immediate share while
  atomically creating or reloading the complete plan. After vote confirmation,
  recover a fresh `CommittedVote`, convert it with `CommittedVote::confirmed`,
  and call `ConfirmedVote::submit_prepared_shares` with the complete current
  fleet. The
  crate validates every payload, rebuilds it with the confirmed VC position,
  and journals delivery before dispatch. After restart, prepare again to load
  the original plan; never replan only missing shares. Re-run `resume_plan`
  after each durable action because later work may depend on on-chain
  confirmations.
  `open_proposals` contains only proposals with no terminal decision yet.

  `needs_delegation_signing` is true for both `Delegate` and
  `AdvanceDelegation`, because locally prepared retries must be signed again.
  The host passes only the new SpendAuth signature to the advancement request;
  the SDK reloads and validates its stored signing context.
  Imported capability bundles yield `AdvanceImportedDelegation`; that path is
  signer-free and poll-only.

The Zcash-format transaction signed during delegation is specified separately
in [Delegation signing transaction (TX1)](docs/delegation-signing-transaction.md).
It distinguishes the PCZT-only signing artifact from the vote-chain
delegation submission and includes software-wallet and Keystone examples.
The companion
[delegation capability handoff](docs/exporting-to-external-software.md)
documents the public-target, delivery-receipt, and zero-migration import flow
for cases where the funds controller and voter are separate parties, including
custody provider integrations.

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
- Replace wallet-local "what comes next" recovery planning with
  `session::resume_plan`; fetch execution material through crate APIs such as
  `vote::CommittedVote::recover` and `share::*`, drive chain work with
  `ChainSubmissionClient`, then keep wallet-specific proof execution and UI
  routing at the wallet boundary.
- Replace wallet-local delegation proof and signing orchestration with
  `delegate::PreparedDelegationBundle`. Callers can use the prepared lifecycle
  for setup, witness completion, proving, signing request construction, signed
  payload assembly, and Keystone request construction.
- Replace wallet-local chain submission with `ChainSubmissionClient`. The SDK
  owns endpoint construction, request encoding, timeouts, retry eligibility,
  polling, exact commitment-tree recovery, and confirmation; hosts supply a
  `ChainTransport`, scheduling, and cancellation. Plain `advance_delegation`,
  `advance_vote`, and `advance_vote_batch` calls are status-only; execute the
  matching local `resume_plan` steps through their `*_with_recovery` methods
  with `ChainRecoveryMode::ExactTree`. Imported delegation advancement remains
  poll-only. Each call performs one bounded pass and returns `Confirmed`,
  `Pending`, `Rejected`, or `Cancelled`. The version-17
  APIs that let callers record transaction hashes, VAN or vote-commitment
  positions, or apply their own parsed chain events have been removed.
- Use `vote::commit` for one singleton. The existing `vote::commit_batch`
  remains as a one-draft compatibility wrapper for singleton submission, while
  `vote::commit_atomic_vote_batch` builds one atomic, ordered multi-question
  transaction. The distinct `SignedVoteCommitments` and `SignedVoteBatch`
  result types keep the singleton and atomic submission endpoints separate.
  Use `vote::CommittedVote::recover` to reload a committed vote and
  `ChainSubmissionClient::advance_vote_with_recovery` with
  `ChainRecoveryMode::ExactTree` for its resumable chain lifecycle.
  Wallets should not write recovery JSON, submission flags, or vote commitment
  positions directly.

Pre-launch wallet databases with older schema versions are reset when opened by
this branch; callers that need to preserve test data should export it before
upgrading the crate.

### Migrating 3.x to 4.0

- Match `NextStepView.kind`, `RoundPlanView.primary_action`, recovery-work
  kinds, and wire `phase` fields as enums; the string tables are gone.
- `VotingDb::open_wallet_sidecar` returns `Arc<VotingDb>` and shares one
  connection per path; drop host-side per-path write locks and
  "database is locked" matching, and branch on `VotingError::kind` (`DbBusy`,
  `PirUnavailable`, `InsufficientEligibility`, ...) or `retryable`.
- Replace hand-rolled Tor transports with one `RouteHttp` implementation and
  `HyperTransport::with_route`.
- Replace `connect_pir_blocking` with `PirFleet::new` plus `with_failover`,
  which orders endpoints and retries only retryable PIR failures.
- Replace per-stage delegation orchestration with `DelegationPipeline` and
  `DelegationSigner`; keep only the seed-owning `SpendAuthSigner`.
- Replace host sequencing of plan steps, and the removed
  `VoteRecoveryExecutor::advance` driver, with `RoundDriver::run`.
  `RoundExecutor::advance_next` and `RoundExecutor::advance_step` are both
  removed: a second way to advance a round from its plan is a second driver.
  Helper shares are submitted through `ConfirmedVote`.
- Replace a host loop over share-tracking passes with
  `ShareTrackingDriver::run`. It repeats a pass on the delay each pass
  computes, keeps every wait inside the round's voting window, and reports why
  it stopped; a host keeps only what it alone can see, such as app lock and
  account identity.
- Configure chain submissions with `ChainSubmissionClientConfig::for_network`.
  Episodes are driven by `RoundExecutor` under a `ChainAdvancePolicy`, not by a
  host polling loop.

The workspace uses the published `voting-circuits 0.12.0-rc.1` release.

## Dependency Strategy

The LRZ backend uses one Ironwood dependency stack:

- **`orchard 0.15`** from [zcash/orchard](https://github.com/zcash/orchard),
  with `unstable-voting-circuits` enabled for the governance proof paths.
- **`pczt 0.9.2`, `zcash_client_backend 0.24.0-rc.7`,
  `zcash_client_sqlite 0.22.0-rc.7`, `zcash_keys 0.16.1`,
  `zcash_primitives 0.30.0`, and `zcash_protocol 0.10.4`** from published
  librustzcash releases.
- **`voting-circuits 0.12.0-rc.1`** from
  [valargroup/voting-circuits](https://github.com/valargroup/voting-circuits)
  for the delegation and vote proof circuits.

`vote-commitment-tree` and `vote-commitment-tree-client` default to Zakura and
select their proving stack through mutually exclusive `zakura`/`lrz` features;
build with `--no-default-features --features lrz` for the LRZ VCT backend.

The published `zcash_voting` crate defaults to Zakura and exposes LRZ through
the mutually exclusive `lrz` feature. Wallet-family selection is consolidated
in published `zakura-wallet-lib 0.1.0-rc4`, whose complete `zakura` and `lrz`
modes never weak-reference both backend families. Gemini selects
`zcash_voting` with `default-features = false, features = ["lrz"]`; Vizor uses
the defaults. External-consumer regression tests verify that Gemini's Cargo
lockfile and resolved metadata contain no Zakura forks.

`Cargo.toml` is the source of truth for version and feature requirements, and
`Cargo.lock` records the exact package sources and versions used by this branch.
The current PIR and IMT releases require Rust 1.91 or newer.

## FFI

Mobile FFI bindings live in [zcash-swift-wallet-sdk](https://github.com/valargroup/zcash-swift-wallet-sdk) (hand-rolled C FFI + Swift wrappers). This repo is a pure Rust workspace.

## License

TODO
