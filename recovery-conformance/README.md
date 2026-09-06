# recovery-conformance

Staging crash-recovery conformance for `zcash_voting`.

## Why this exists

The crate's ~1200 unit tests prove that durable rows are written in the right
order. They cannot prove the claim those rows exist to support: that an app
**killed** mid-round — no unwinding, no `Drop`, no flush, no graceful SQLite
close — restarts against the same sidecar and the same live chain and
converges, without spending a note twice or losing a vote.

Every existing test ends in a clean `drop(db)`, and a clean drop is the one
thing a crash is not. `docs/chain_submission_invariants.md` even lists "a
process crash while a `Submitting` reservation exists" as possibly-dispatched,
and specifies `abandoned Submitting on restart -> Recovering`. Nothing killed a
process to check.

This package does. It provisions a real multi-proposal, multi-bundle round on
staging, drives it in a child process, `abort()`s that child at a named durable
boundary, then reopens the same sidecar and asks the only question that
matters: **does the round still know what it owes?**

The oracle is `session::resume_plan`, a pure function of durable state. Nothing
in memory survives the crash, so the plan after reopen *is* the complete
definition of the remaining work.

## Credentials

No secret lives in this repository. Values are read from the process
environment at run time, and Infisical is the source of truth.

| What | Where |
| --- | --- |
| Vote-manager key for `svote-1` round creation | Infisical `vote` project `40862c6d-a089-4355-b405-0477be0ee3b1`, key `VOTE_MANAGER_VOTE_SDK`, present in `dev`/`staging`/`prod`; the suite uses **`staging`** |
| Voting wallet seed (fixed across runs) | Infisical, same project, key `VOTE_SDK_VOTER_TEST`, present in `dev`/`staging`/`prod` |

`VOTE_SDK_VOTER_TEST` is the fixed voter's **24-word BIP39 mnemonic** — a Zcash
wallet seed derived through ZIP-32, not a cosmos key. Its notes are what every
round delegates. All three credentials have different shapes (24 words, 12
words, 64-char hex), so no parsing path is shared.

`VOTE_MANAGER_VOTE_SDK` is a **12-word BIP39 mnemonic** (the older
`VOTE_MANAGER` is a 64-char hex seed — different shape, don't reuse the parsing
path). Derive with secp256k1 over **`m/44'/133'/0'/0/0`** — Zcash's coin type, **not**
the cosmos default of 118 — and bech32 prefix `sv`. Verified against staging:
133 reproduces the registered coordinator
`sv1z4rawnk8ny0pzsewyzm3egdd7296fr8p20fkf8`; 118, 60 and 1 each produce a
well-formed address for an account the chain has never seen.

It is the scoped coordinator key. It authorizes
`MsgCreateVotingSession`, which the chain restricts to the vote manager
(`ValidateVoteManagerOnly` in `x/vote/keeper/msg_server.go`). It is **not** an
attestation key: the suite self-signs the dynamic config it trusts rather than
having anyone attest a throwaway round.

Run the suite under Infisical so nothing touches disk:

```bash
infisical run --env=staging -- make recovery-conformance
```

Check a key is present without printing it. Test the **value**, not the exit
code: `infisical secrets get` returns 0 for a key that does not exist and
substitutes the literal `*not found*`, so an `rc`-based check reports every key
as present.

```bash
infisical secrets get VOTE_MANAGER_VOTE_SDK --env=staging -o json \
  | grep -q '"secretValue":"\*not found\*"' && echo absent || echo present
```

`~/agent-global.env` may hold a namespaced `VOTE_DEV__*` copy from an earlier
sync. That file is a point-in-time snapshot and goes stale as soon as a key is
added or rotated — it does not currently contain `VOTE_MANAGER_VOTE_SDK`. The
suite reads live values rather than that cache.

## Running it

Not part of `make test`, and not in CI: it needs the network and it kills
processes.

```
make recovery-conformance-check   # type-check and lint
make recovery-conformance         # run against staging (slow)
```

## Crash stages

Each stage sits immediately next to a durable commit. `touches_chain()`
partitions them: stages before the first POST leave staging untouched and can
branch from one provisioned round by copying the sidecar; the rest each need a
round of their own, because a delivered transaction cannot be rewound.

| Stage | Durable state it leaves |
| --- | --- |
| `before-delegation` | bundles only |
| `after-note-selection` | bundles only (selection writes nothing) |
| `after-pczt` | `bundles.pczt_sighash` + TX1 effects, write-once |
| `after-proof` | `proofs` row |
| `after-signing` | proof + any Keystone signature |
| `before-broadcast` | `chain_submissions` `submitting`, bytes never sent |
| `after-broadcast-unread` | `submitting`, transaction may be on chain, no hash |
| `after-broadcast-read` | as above; the real response is captured for the parent |
| `after-tracking` | `tracking` + candidate hash |
| `before-cast` | delegation confirmed |
| `after-tree-sync` | delegation confirmed; cached tree synced |
| `after-vote-proof` | nothing new — the proof is lost by design |
| `after-vote-commit` | `votes.commitment_bundle_json`, no POST reserved |
| `after-helper-plans` | `helper_share_plans` + `round_immediate_share` |
| `before-vote-broadcast` | vote `submitting`, bytes never sent |
| `after-vote-broadcast` | vote `submitting`, POST dispatched |
| `after-vote-confirmed` | `confirmed` + `votes.vc_tree_position` |
| `before-share-post` | `share_delegations.attempting_urls` |
| `after-share-post` | `attempting_urls`; helper answered, outcome unwritten |
| `after-share-accepted` | `sent_to_urls` |

### The two sharp cases

- **`before-broadcast`** — nothing was sent, yet the abandoned reservation must
  normalize to `Recovering`, **not** disappear: a restarted process cannot
  prove the bytes never left. Conservative by design, and the single most
  valuable test here.
- **`after-broadcast-unread`** — the transaction is on staging and the wallet
  has no hash for it. Resume must reach `Confirmed` by exact-tree scanning and
  must never POST a second transaction spending the same notes.

## Round consumption, and what that costs

A delegation is consumed **on the vote chain**: once a bundle's delegation is
registered for a round, that round's gov nullifier is spent and the bundle
cannot delegate again. The Zcash notes themselves are untouched — TX1 is a
PCZT-only signing artifact and is never broadcast — so the voting wallet never
needs re-funding. It is the *round* that is one-shot, not the money.

Two consequences shape the suite:

1. **A stage that gets a POST onto the wire consumes its round.** Those stages
   each need a freshly provisioned round. `touches_chain()` names them.
2. **Driving a resumed round to quiescence is itself mutative**, even when the
   crash was pre-broadcast. A `before-broadcast` crash leaves a `Recovering`
   row that resume will dispatch, which consumes the round just the same.

So the split is not "before/after the crash" but "does this test *mutate*":

| Tier | Stages | Round | Asserts |
| --- | --- | --- | --- |
| Non-mutative | everything with `!touches_chain()` | one provisioned round, branched by copying the sidecar | crash, reopen, re-plan and durable state (A1 and the stage's own row assertions). Stops at inspection. |
| Mutative | everything else, plus any test that drives to quiescence | one fresh round each | the above, then resume to quiescence (A2, A3, B2, B3) |

This is what keeps the suite affordable. There is no mock prover — the
`test-fixtures` seeding helpers skip proving, which is exactly what this suite
must not do — so a 3-bundle, 2-proposal round costs 3 ZKP-1 and 6 ZKP-2 proofs,
each minutes. The non-mutative tier pays that once for the whole group instead
of once per stage.

## The fixed voter wallet

`VOTE_SDK_VOTER_TEST` holds **11 notes, which bundle into 3**. That shape is
deliberate and worth not breaking.

Bundling packs notes value-descending, five to a bundle
(`BUNDLE_NOTE_SLOTS = 5`), so 11 notes fill 5/5/1. The privacy trim then tries
to shed the smallest bundle down to `DEFAULT_MAX_PRIVACY_BUNDLES = 2`, but it
may only spend `DEFAULT_PRIVACY_DROP_BPS` — 1% of selected value. With
near-equal notes the last bundle is about 9% of the balance, far over budget,
so the trim breaks immediately and all three survive. The lone note in bundle 3
must still be worth at least `BALLOT_DIVISOR` (0.125 TAZ) by itself or step 4
drops it as sub-ballot.

Three bundles rather than two is what makes the multi-bundle invariants real:
`E1` crashes one bundle mid-proof and asserts the others are untouched, which
needs a bundle to spare. The cost is one extra delegation proof and one extra
vote proof per proposal.

Because bundling is value-sensitive, rebalancing this wallet can silently
change the bundle count and quietly weaken `E1`. The suite asserts the layout
it expects at provisioning time rather than trusting it.

## Which chains this runs against

| | |
| --- | --- |
| Zcash | **testnet**, via `https://testnet.zec.rocks:443` |
| Vote chain | `svote-1` (staging), RPC `https://stage.vote-rpc-primary.valargroup.org` |
| Coordinator | `sv1z4rawnk8ny0pzsewyzm3egdd7296fr8p20fkf8`, derived at `m/44'/133'/0'/0/0` |

Two things here are easy to get wrong, and both fail in ways that look like
recovery bugs rather than configuration:

- **Stardust is mainnet-only.** Every Stardust host reports `chainName:
  "main"`, and no testnet Stardust exists. Pointed at one, the voter wallet
  finds no notes and the run reports "no eligible notes".
- **Read the published config, never a local checkout.** The
  `token-holder-voting-config` working copy can be stale; its `stage/pir.json`
  named a snapshot height that was a plausible *mainnet* height while the
  published one was unambiguously testnet.

## Invariants under test

### A. Recovery oracle

- **A1** The plan is a total function of durable state — two `resume_plan`
  calls agree, and agree with what the child last read.
- **A2** No crash produces an unrecoverable round: driving to quiescence after
  reopen never ends in `Failures` or `PassBudgetExhausted`.
- **A3** Convergence is stage-independent — the terminal
  `RoundRecoverySnapshot` after crash-and-resume equals an uncrashed control
  run's, field for field. The strongest assertion in the suite.
- **A4** Resume is idempotent: crashing twice converges where crashing once did.
- **A5** Recovery never fabricates a tree position — a helper share is never
  sent with the `0` placeholder that `round_snapshot` may display.

### B. Chain submission (`docs/chain_submission_invariants.md`)

- **B1** An abandoned `Submitting` row normalizes to `Recovering` on restart.
- **B2** A crash after dispatch never causes a second spend — exactly one
  transaction per generation is accepted, verified against the chain.
- **B3** Exact-tree recovery resolves a hashless dispatch to `Confirmed` with
  `confirmation_source = 'tree'`.
- **B4** Terminal rows are immutable across a crash.
- **B5** `committed_post_reservations` is monotone across crash and resume.
- **B6** Generation identity is immutable; two generations never claim one hash.

### C. Round orchestration (`docs/round_orchestration_invariants.md`)

- **C1** Undispatched work follows the current roster; dispatched work survives
  a roster change.
- **C2** Vote work resumes per proposal without cross-contamination.
- **C3** A listed step always resolves — no reopened plan yields
  `InvariantViolation`.
- **C4** The ballot gate survives a crash: an undecided round re-plans to
  `NeedsBallot`, never to `CastVote`.
- **C5** A committed vote is never broadcast without durable helper plans.
- **C6** A crash during tree sync leaves a consistent cached tree or none.
- **C7** Helper plans and the immediate-share designation stay bound to their
  generation.

### D. Helper submission (`docs/helper_submission_invariants.md`)

- **D1** `attempting_urls` is a durable crash marker; recovery treats that
  helper as interrupted, not untried.
- **D2** A recovery re-POST never downgrades a durable acceptance.
- **D3** Ambiguity is never erased by a later definite failure.
- **D4** A reloaded plan keeps its original target count.
- **D5** Only definite-delivery deficits are resumed.

### E. Multi-bundle and multi-proposal

- **E1** Failure isolation is per bundle across a crash.
- **E2** Vote work stays proposal-primary after resume.
- **E3** No round lock leaks across the crash.
- **E4** `RoundWorkTally` reports run-relative progress after resume.

## What this suite cannot cover

- **Atomic vote batches.** `ATOMIC_VOTE_BATCHES_ENABLED = false`
  (`zcash_voting/src/lib.rs`) while no deployed chain serves `cast-vote-batch`,
  so a fresh staging round only ever produces singleton casts. Batch
  classification and recovery stay covered by unit tests. When the route ships,
  the vote stages gain a batch variant and the atomicity invariant becomes
  testable here; nothing else changes.
- **The mid-ZKP-2 proof.** Nothing is durable between `prepare_vote_work` and
  `persist_prepared_vote_work`, so `after-vote-proof` costs minutes of
  re-proving. The suite asserts that this is *only* a cost — no durable damage,
  no orphaned lock, no partial tree.
- **Rewinding staging.** A delegation consumed on the vote chain stays
  consumed, which is why every mutative stage needs its own round. See
  [Round consumption](#round-consumption-and-what-that-costs).

## Design notes

**Why a child process.** Both provers run on dedicated 64 MiB-stack OS threads
that are deliberately not cancellable, and they hold the round lock through a
cloned `Arc` so it outlives a dropped future. An in-process crash model would
leave a live prover still holding that lock and still writing to the sidecar the
"restarted" run had just reopened — corrupting the state under test. Killing the
process is what makes the detached prover go away.

**Why the round lock does not help us.** `vote_work::round_lock::ROUND_LOCKS` is
a process-global map. It gives no cross-process exclusion, so safety across the
crash rests entirely on SQLite locking plus the `chain_submissions` triggers and
unique indexes — which is exactly what should be under test.

**Why a missed stage is a failure.** A child that finished the round would
satisfy every assertion about "the state a crash left", because the state
inspected would simply be a completed round. `run_until_crash` therefore
requires `SIGABRT` at the armed stage and rejects the
`EXIT_STAGE_NEVER_REACHED` exit, so a stage that stops firing fails loudly
instead of decaying into a no-op.
