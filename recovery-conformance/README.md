# recovery-conformance

Staging crash-recovery conformance for `zcash_voting`.

## Why this exists

The crate's ~1400 unit tests prove that durable rows are written in the right
order. They cannot prove the claim those rows exist to support: that an app
**killed** mid-round — no unwinding, no `Drop`, no flush, no graceful SQLite
close — restarts against the same sidecar and the same live chain and
converges, without spending a note twice or losing a vote.

Every unit test ends in a clean `drop(db)`, and a clean drop is the one thing a
crash is not. `docs/chain_submission_invariants.md` lists "a process crash while
a `Submitting` reservation exists" as possibly-dispatched and specifies
`abandoned Submitting on restart -> Recovering`; nothing killed a process to
check.

This package does. It provisions a real multi-proposal, multi-bundle round on
staging, drives it in a child process, `abort()`s that child at a named durable
boundary, then reopens the same sidecar and asks the only question that matters:
**does the round still know what it owes?**

The oracle is `session::resume_plan`, a pure function of durable state. Nothing
in memory survives the crash, so the plan after reopen *is* the complete
definition of the remaining work.

## Running it

Not part of `make test` and not in CI: it needs the network and it kills
processes.

```bash
infisical run --env=staging -- make recovery-conformance   # the full matrix
make recovery-conformance-check                            # type-check and lint
```

To re-run only the stages a change could affect:

```bash
infisical run --env=staging -- env \
  RECOVERY_CONFORMANCE_STAGES=after-vote-broadcast,after-vote-confirmed \
  make recovery-conformance
```

The control run is unconditional whatever the selection, because every terminal
comparison is against it. An unrecognized stage name fails the run rather than
selecting nothing, and a matrix that attempts no stages, or passes none of the
stages it attempts, fails as well: a green run over untested ground is the way
a suite like this rots.

### Credentials

No secret lives in this repository, and none is written to disk. Two values are
read from the environment at run time:

| | |
| --- | --- |
| `VOTE_MANAGER_VOTE_SDK` | Scoped coordinator key. Authorizes `MsgCreateVotingSession`, which the chain restricts to the vote manager. **Not** an attestation key — the suite self-signs the dynamic config it trusts. |
| `VOTE_SDK_VOTER_TEST` | The fixed voter's 24-word BIP39 mnemonic. |

Read them live from Infisical on every run. Do not point the suite at a local
snapshot of exported secrets: such a file goes stale the moment a key is added
or rotated, and a stale value fails as a rejected authorization, which reads
like a permissions problem rather than a wrong key.

To check a key is present without printing it, test the **value**, not the exit
code — `infisical secrets get` returns 0 for a key that does not exist and
substitutes the literal `*not found*`:

```bash
infisical secrets get VOTE_MANAGER_VOTE_SDK --env=staging -o json \
  | grep -q '"secretValue":"\*not found\*"' && echo absent || echo present
```

## Crash stages

Each stage sits immediately next to a durable commit. `touches_chain()`
distinguishes whether the armed run itself may have submitted anything. Every
matrix case still gets its own round, because the post-crash resume drives that
round to quiescence and therefore eventually submits even after a pre-POST
crash.

| Stage | Durable state it leaves |
| --- | --- |
| `before-delegation` | bundles only |
| `after-note-selection` | bundles only; notes chosen, nothing written yet |
| `after-pczt` | `bundles.pczt_sighash` + TX1 effects, write-once |
| `after-proof` | `proofs` row |
| `after-signing` | proof + any Keystone signature |
| `before-broadcast` | `chain_submissions` `submitting`, bytes never sent |
| `after-broadcast-unread` | `submitting`, transaction may be on chain, no hash |
| `after-broadcast-read` | as above; the real response is captured for the parent |
| `after-tracking` | `tracking` + candidate hash — needs a one-pass chain policy, see below |
| `before-cast` | delegation confirmed |
| `after-tree-sync` | delegation confirmed; cached tree synced |
| `after-vote-proof` | nothing new — the proof is lost by design |
| `after-vote-commit` | `votes.commitment_bundle_json`, no POST reserved; the seam is the fleet preflight |
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

## Invariants

Split by whether the suite actually asserts them today. Everything in the
second table is a property the SDK is designed to hold that this suite does
**not** yet check — listed so the gap is visible, not as a claim.

### Asserted by the matrix

| | Assertion | Where |
| --- | --- | --- |
| **A1** | Two `resume_plan` calls over the same durable state return identical plans | `deterministic_plan` |
| **A2** | A resumed round reaches terminal quiescence, never `Failures` or `PassBudgetExhausted` | matrix, per stage |
| **A3** | Terminal submission states match the uncrashed control | matrix, per stage |
| **A4** | A second resume plans no further *foreground* work. `ConfirmShare` steps are excluded by design: a round ending in `BackgroundShareWorkOnly` has finished what the foreground owns, and the host's timer closes the rest | `assert_idempotent` |
| **B1** (part) | After a `before-broadcast` crash the abandoned reservation **still exists**; a row that vanished would let the next pass reserve a fresh first attempt and build a second transaction | `assert_stage_state` |
| **B2** | The crashed bundle is *advanced*, never re-delegated — a second delegation would spend its notes twice | `assert_stage_state` |
| **B4** | Terminal rows (`confirmed`/`rejected`/`submitted_without_hash`) are byte-identical across resume | `assert_terminal_rows_unchanged` |
| **B5** | `committed_post_reservations` never decreases | `assert_reservations_monotonic` |
| **C5** | A vote submission never exists without a durable helper plan | `assert_plans_precede_broadcast` |
| **E1** | Bundles other than the crashed one keep their pending work | `assert_other_bundles_untouched` |
| **B3** (part) | A hashless dispatch confirms by one of the two routes open to it, tree scan or same-generation retry, and the route is reported every run. Which one wins is the SDK's choice and not assertable; that no *second* generation was built is what carries the safety claim | `assert_confirmed_by_a_legal_route` |
| **B2/B3** (identity) | Where the stage captured the dispatched response, the transaction the round finally confirms is **the same one** the killed process had already sent — not merely "exactly one eventually confirmed" | `assert_recovered_the_same_transaction` |
| **D2** (part) | `after-share-accepted` records a definite acceptance in `sent_to_urls` | `assert_stage_state` |
| — | Each stage leaves the durable state its row expects (PCZT persisted, proof persisted, vote committed, share journaled) | `assert_stage_state` |

### Not asserted yet

These need either a stage that reliably reaches them or an assertion that does
not exist. **Do not read them as tested.**

| | Why not yet |
| --- | --- |
| **A5** | No assertion that a helper share is never sent with the `0` tree-position placeholder. |
| **B1** (rest) | That the row becomes exactly `Recovering` is **not** asserted: normalization is lazy, happening inside the lifecycle's next admission rather than at open, so it needs an assertion that drives one admission and reads the row before the round advances past it. The safety-critical half — the row survives at all — *is* asserted. |

| **B6** | Generation-identity immutability is trigger-enforced but not checked here. |
| **C1, C2, C4, C6, C7** | Roster changes, per-proposal isolation, the ballot gate, tree-cache consistency, and generation binding have no assertion. |
| **D1** | **Asserted and passing.** Was failing; see the closed finding below. |
| **D3-D5** | Ambiguity erasure, reloaded-plan target counts, and resuming only definite-delivery deficits are still unasserted. |
| **E2, E3, E4** | Proposal-primary ordering, round-lock leakage, and run-relative tally are unasserted. |

### The sharp submission cases

`before-broadcast` and `after-broadcast-unread` are where a bug costs a voter a
note. Three things are checked, in increasing strength:

1. the crashed bundle is planned for *advancement*, never re-delegation;
2. reservation counts are reported per stage and asserted monotonic — every
   committed POST increments one, so the count is how many times the wallet
   committed to sending;
3. where the stage captured the response, the confirmed transaction hash must
   equal the dispatched one. A round that quietly sent a replacement would
   confirm a *different* hash while looking equally healthy, which counting
   alone cannot detect.

A stalled recovery is not a verdict: the specification separates
`ChainRecoveryStalled` from `ChainTerminal` because running again later may
resolve it, so the matrix waits and re-drives rather than failing.

Neither is a stale vote-tree cache. A crash can leave the cached
vote-commitment tree disagreeing with a delegation that confirmed; the tree sync
detects that, **discards the cache**, and fails the pass so the next one
re-syncs from scratch. That is the SDK repairing itself, so the matrix
re-drives. The rule is deliberately narrow — matched on that one condition —
because a broader one would retry past real findings.

### Reaching the tracking window

`ChainOutcome` is reported once per `advance_step`, at the end, and carries the
episode's *terminal* outcome — not one event per poll. Under the shipped
45-pass policy an episode polls until the submission confirms, so a stage
waiting to observe one *still tracking* can never fire. The run therefore arms a
single-pass chain policy for `after-tracking` alone; every other stage keeps the
shipped cadence, so the control it is compared against is unaffected.

### Reaching after-vote-commit

`after-vote-commit` names a narrow boundary — a committed vote whose helper
plans are not yet durable — and the progress stream does not mark it: casting
goes from `VoteCommit(Signing)` straight to `HelperPlansPrepared`, which is
already the *next* stage's boundary.

The seam is not in the event stream but in the work. Vote completion probes the
helper fleet between those two commits, and that probe is a real network round
trip through the transport this suite already wraps, so a crash on it lands
squarely in the window with no production change.

The stage was documented as unreachable and skipped in every run before this
was noticed. Nothing is excused from firing now: a stage that does not crash
where it claims to fails the matrix.

## What this suite cannot cover

- **Which route resolves a hashless dispatch.** A crash between dispatch and
  response leaves no candidate hash, and recovery may resolve it either by
  scanning the tree or by re-POSTing the same generation and being handed the
  hash back. Both are specified. Requiring the tree route appeared to work for a
  long time only because the crash boundary was wrong: aborting after the whole
  response had been read let the chain include the transaction first, so the
  tree won the race. At the real boundary the retry usually wins, and waiting
  for block inclusion before resuming does not change it. The route is printed
  every run so a change is visible; the safety claim rests on no second
  generation being built, which is asserted.

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

## Environment

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

A wallet learns a round's parameters from the signed dynamic config: it fetches
the document, verifies a `RoundAuthPayloadV2` signature over the round id,
`ea_pk`, and PIR layout, and only then trusts the values. **This suite skips
all of that** and reads the round straight off the chain that created it
(`provisioning::fetch_round`).

That trade is sound here and nowhere else. Config authentication answers "is
this round genuine and endorsed" — a question about trusting a third party's
document. The suite provisions the round itself, minutes earlier, with its own
coordinator key, so it already knows the answer; signing a document to verify a
fact it just created would exercise the config layer rather than recovery.

What is *not* skipped is agreement: the parameters used to drive the round come
from the chain's own record, so a provisioning mistake surfaces as a mismatch
instead of being carried forward by a local copy.

## Round consumption, and what that costs

A delegation is consumed **on the vote chain**: once a bundle's delegation is
registered for a round, that round's gov nullifier is spent and the bundle
cannot delegate again. The Zcash notes themselves are untouched — TX1 is a
PCZT-only signing artifact and is never broadcast — so the voting wallet never
needs re-funding. It is the *round* that is one-shot, not the money.

Two consequences shape the suite:

1. **A stage that gets a POST onto the wire consumes its round.** Those stages
   need a freshly provisioned round. `touches_chain()` names whether the armed
   run can do so before crashing.
2. **Driving a resumed round to quiescence is itself mutative**, even when the
   crash was pre-broadcast. A `before-broadcast` crash leaves a `Recovering`
   row that resume will dispatch, which consumes the round just the same.

Therefore every matrix case provisions a distinct round. Sharing a round among
pre-POST crash stages would still let the first case's resume consume that
round, making every later case observe chain effects that its sidecar does not
own. There is no mock prover — the `test-fixtures` seeding helpers skip
proving, which is exactly what this suite must not do — so this isolation is
expensive but required for the terminal convergence oracle.

## Verification record

Three consecutive runs of the full matrix against staging, every stage
exercised and none skipped:

| Run | Result | Duration |
| --- | --- | --- |
| 1 | 20 passed, 0 failed, 0 skipped | 2938s |
| 2 | 20 passed, 0 failed, 0 skipped | 3082s |
| 3 | 20 passed, 0 failed, 0 skipped | 3167s |

Each run provisions a control round plus one per stage — roughly twenty fresh
rounds, sixty across the three, every delegation and vote real and on
`svote-1`.

Under test: this crate, plus **two sets of SDK fixes that are not on this
branch**. The runs are honest only with both named, and this branch does not
pass without them.

- The seven `votes.tx_hash` completion fixes, which this suite found and which
  are in review separately. Without them the vote and share stages cannot
  converge at all.
- The delegation-proof phase fix that `52d0229c` carries on another branch.
  Without it `after-signing` fails deterministically: a bundle persisting its
  proof after another bundle advanced the round-wide phase to `VoteReady` is
  refused as a phase regression.

Both need to land before this branch's own base turns the matrix green. Running
it here today reproduces the defects rather than passing over them, which is
the suite working as intended, not a broken branch.

### What these runs are worth

Earlier runs of this matrix also reported green, and meant considerably less. A
review found four ways a run could pass over ground it had not tested: the
matrix could skip every stage and still report success, the no-second-POST
claim was printed rather than asserted, the control comparison read only
submission state names, and two crash boundaries did not fire where their names
said. The runs above are the first taken after those were closed, so they are
not a repeat of the earlier ones at a higher number — they are the first that
carry the weight the table implies.

### What it caught

Seven SDK defects, all from one root cause — `votes.tx_hash` read as proof a
vote finished, when a vote confirmed by an exact-tree scan never carries one.
Three failed closed and stalled rounds; three failed **open**, permitting a
rebuild of an on-chain vote, a conflicting ballot intent, and silent overwrite
of an on-chain vote. A seventh erased the helper attempt marker in release
builds only, invisible to a suite running with debug assertions on.

Plus an intermittent multi-bundle phase regression: a bundle persisting its
delegation proof after another advanced the round phase to `VoteReady` was
refused. It passed three earlier runs, then failed twice consecutively once
coverage was complete — the kind of race this matrix exists for.

Each fix carries its own commit and hermetic regression test; the specifications
in `docs/` state the rules they restored.

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
