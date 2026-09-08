# Changelog
All notable changes to this workspace will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this workspace adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

This release is `zcash_voting` 4.0.0.

### Added

- Keystone signing requests retain the exact transaction across background ZKP1
  warmup and wallet restart. The SDK validates and loads the signing transaction
  from one database snapshot, preventing concurrent setup replacement from
  mixing targets. Schema 22 adds its storage without repairing legacy round state.

- `round_drive` composes `RoundExecutor` calls into one run: `RoundDriver::run`
  re-plans from durable state, dispatches the obligations the plan lists, paces
  a still-tracking submission by `RoundDrivePolicy::pending_repoll`, isolates
  failures per bundle, and returns a `RoundRunReport` whose `RoundQuiescence`
  names the state only the host can resolve — an open ballot, delegation
  signatures it does not hold, a terminal or stalled chain submission,
  background share work, cancellation, or nothing left to do. `RoundHostSource`
  supplies the host context once per dispatch rather than once per run, so a
  long proof cannot leave the following step planning against a stale clock,
  and `RoundDriveEvent` names the step every observation came from, which a
  bare `RoundStepProgress` does not. `RoundWorkTally` reports selected-vote
  submission progress from obligation membership, so an atomic batch counts
  as every proposal in it rather than as its anchor alone. Independent
  bundle-locked delegation work runs up to
  `RoundDrivePolicy::max_bundle_concurrency`, while round-locked work and
  `StopRound` failure isolation remain serial. Dispatch-budget reports are
  taken from a fresh plan after the last allowed wave, and absent per-bundle
  stored Keystone signatures stop before dispatch as a signature handoff.
  The stop reason is decided from what the run can still dispatch rather than
  from a round-wide flag, so a persisted terminal submission, a bundle a
  failure isolated, missing bundle setup and an unfinished ballot are each
  reported in their own right instead of being polled past. A signature handoff
  names every bundle the round still owes a delegation for, not only the ones
  the current wave would run, so a voter signs once rather than a wave at a
  time and nothing is dispatched before the first of them, and every admitted
  admitted bundle is judged by its own signer context rather than one mode
  standing for the wave. A
  failure keeps the durable effects its step already made — the chain outcome
  it observed, and, through the new `RoundStepFailure::delegation`, a bundle it
  signed before losing the chain. A wave-ending report refreshes its plan and
  tally from durable state, so it describes the round the run left rather than
  the one it found.
  Driver dispatches
  inherit the run's operation epoch, so a host that switches session or account
  while the driver is planning interrupts the step instead of having it adopt
  the new epoch. `round_lock::bundle_scope` is the single definition of which
  lock a step takes, read by both the executor that locks and the driver that
  schedules. Hosts previously wrote this loop
  themselves; `docs/round_orchestration_invariants.md` specifies it. `wire`
  carries the host-facing projections — `RoundRunReportView`,
  `RoundQuiescenceView`, `RoundWorkTallyView`, `RoundDriveEventView` and
  `RoundStepFailureRecordView` — in the same flat, serde-stable shape the other
  views use, including the signed delegation bundles a run produced, so a
  cross-language binding sees what a native caller sees. The prelude exports
  the driver types. `RoundRunReport` and `RoundStepFailureRecord` are
  `#[non_exhaustive]`: hosts read a report, never build one.
- `RoundDrivePolicy::progress_baseline` chooses what a run's progress total is
  measured against. `ProgressBaseline::Run`, the default, keeps the historical
  run-relative total: a round resumed with two questions left reports two.
  `ProgressBaseline::SelectedChoices` counts every durable selected choice
  whose vote belongs to the current roster or chain lifecycle. Skips and
  clearable stale choices are excluded because they owe no vote submission.
  With unchanged selections and roster, this keeps the selected-vote total
  across a quit and reopen. Both baselines are captured from the run's first
  plan and share one completion measure — a proposal is done once no `Cast`
  and no `ReconcileChain` obligation covers it and the plan did not have to
  withhold its cast — so only the denominator differs.
  The choice belongs to the host because it depends on what the host's label
  claims to be counting, which the driver cannot know.
- `RoundPlan::needs_bundle_setup` reports a round that holds a ballot choice
  but has no bundle rows yet. Eligibility checks do not persist a bundle
  plan, so a host that records a ballot before running setup previously made
  every later `resume_plan` call fail with `InvalidInput`, leaving the round
  unvotable and naming no remedy. Planning now reports the condition with a
  `Delegate` primary action and no steps; persist the bundle plan and plan
  again.

- `RoundPlan::delegation_bundles_needing_work` and
  `RoundPlan::delegation_bundles_needing_signing` are the per-bundle form of
  `needs_delegation_signing` and `has_in_flight_delegation`, ascending and
  deduplicated. Any delegation step puts its bundle in the first; `Delegate`
  and `AdvanceDelegation` put it in the second, because advancing a delegation
  in flight re-signs its locked generation, while an imported delegation is
  already broadcast and never asks the voter for a signer. A host showing
  delegation progress per bundle reads these rather than filtering
  `delegation_statuses` against rules the planner already applied, which is how
  a host drifts from the planner.

- `RoundExecutor` executes any planner `NextStep` for a bound round: it
  proves and signs delegations through a `DelegationDriver`, re-signs and
  advances in-flight delegations, casts every draft of a bundle (tree sync,
  VAN witness, proofs off the async runtime, persistence, helper plans, chain
  advancement, and share delivery once confirmed), resumes persisted vote
  work, and runs focused share confirmation. Delegation steps lock per
  bundle; chain and share steps lock per round. `RoundHostContext` carries
  the per-call host inputs, an ordered list of vote-tree node URLs that
  `CastVote` fails over across (dropping the cached tree after every failed
  node), and
  derives last-moment timing from the shared share policy; `set_ballot_intents` records decisions against the bound
  roster.
- `DelegationPipeline` binds the sidecar, a `WalletDbOpener`, the round's
  lightwalletd inputs, account, hotkey, and bundle policy once and runs bundle
  setup, eligibility, PIR precompute, proof generation, Keystone requests, and
  prove-and-sign. `DelegationSigner` carries a host `SpendAuthSigner` callback
  or a stored or provided Keystone signature, so seed material never enters
  the crate. `start_proving_cache_warmup` starts the process-lifetime key
  warm-up once.
- `DelegationStatus::terminal` (and its wire view) states outright that a
  bundle's delegation ended without a confirmation and no further step will
  be planned for it; a confirmed bundle is a success, not terminal.
  A host cannot derive this from the phase: the wallet-facing
  `WorkflowPhaseView` reports a dispatch that reached the chain without a
  usable transaction hash the same way it reports a healthy submission, and
  retrying that would resubmit. `submission_diagnostic` says which terminal
  outcome it was.
- `RoundPlan::has_unconfirmed_shares` says whether a round still owes a helper
  share, so a host can decide whether to start share tracking at all without
  holding durable share rows. When to run each pass is `ShareTrackingDriver`'s;
  this release does not ship a host-side way to derive that schedule.
- `share_tracking_drive` composes `track_pending_shares` passes into one run:
  `ShareTrackingDriver::run` repeats a pass until a round's helper shares are
  quiescent, waiting exactly the delay each pass computed and supplying a
  cadence of its own only after a failed pass, which computed none. It is the
  share-side counterpart of `round_drive` and mirrors its shape: a
  `ShareTrackingDrivePolicy`, a `ShareTrackingHostSource` read once per pass so
  a refreshed helper fleet or clock reaches the next one, a synchronous
  `ShareTrackingReporter` over `ShareTrackingEvent`, and one
  `ShareTrackingRunReport`. Passes are strictly sequential, because each plans
  from the share rows the previous one wrote. `ShareTrackingQuiescence` is
  exhaustive over why a run stopped, so a host acts on it rather than
  re-reading share rows: `NothingToTrack`, `AllConfirmed`, `VoteEndReached`,
  `Cancelled`, `AlreadyDriving`, `Failing`, and `PassBudgetExhausted`. A round
  — keyed by sidecar, wallet, and round id — admits one run: a second started
  while a *live* one is in flight stops with `AlreadyDriving` rather than
  doubling the round's helper traffic, while one started against a *departing*
  holder — cancelled, or left behind by an epoch change — waits for it to
  release the round and takes over, so cancelling a run and starting its
  replacement cannot leave the round undriven. Each pass acts under the wallet
  its run was admitted for. `ShareTrackingReport` gains `unconfirmed_at_entry`,
  what the round owed when a pass began — absent when the pass failed before it
  could look — which is what separates `NothingToTrack` from `AllConfirmed`. The
  run report accumulates which helpers a share reached rather than how many
  times, since recovery re-sends an overdue share to helpers that already
  accepted it, and the per-pass events keep every attempt. Vote
  end is what bounds a healthy run — every wait is shortened to what is left of
  the window, and `max_passes` is `None` by default, because a pass count is
  not a duration and any budget expires inside a multi-day voting window. A
  failed pass hands back what it had already committed, so a run's report and
  its `PassFailed` event keep durable progress no later pass could rediscover.
  Cancellation and an
  operation-epoch change are observed between passes, during the wait, and
  inside a pass through its cancel callback, and the wait is woken by the
  control rather than polled, so a wait that spans the hours until a delayed
  share is due costs one timer and a host draining a run does not wait it out.
  Wire views — `ShareTrackingRunReportView`, `ShareTrackingQuiescenceView`,
  `ShareTrackingEventView`, `ShareTrackingPassReportView`, and
  `ResubmittedShareView` — follow the same flat, serde-stable shape as the
  round-drive views.
- `PirFleet`, `PirSession`, and `PirProofSource`: ordered PIR endpoints with
  failover on typed retryable failures, serviced from a dedicated thread so
  proving can run inside another runtime's blocking pool.
- `wire::PirSnapshotEndpointStatusView` and
  `wire::PirSnapshotEndpointDiagnosticView` project a PIR snapshot-height probe
  for hosts. `pir_snapshot` already reported which endpoints could answer for a
  round's snapshot and why the others could not; the diagnostics had no wire
  form, so a cross-language host could see that endpoint selection failed but
  not whether a server was behind, ahead, or unreachable.
- `RouteHttp`: a request-level executor a host implements once to route every
  SDK transport (PIR, tree sync, helper, vote chain) through Tor or a proxy.
  `HyperTransport` is generic over it; `DirectRoute` is the SDK default.
  Classification of definite versus ambiguous failures is derived from the
  executor's dispatch hook in one place.
- `ChainSubmissionClient` composes bounded passes into one episode under a
  `ChainAdvancePolicy`, driven by `RoundExecutor` rather than by a host;
  `ChainSubmissionClientConfig::for_network` and
  `Network::default_vote_chain_id` replace host-side literals.
- `VotingError::kind`, `VotingError::retryable`, and the new variants
  `InsufficientEligibility`, `NoSpendableNotes`, `SetupAlreadyPersisted`,
  `DbBusy`, and `PirUnavailable`, with `wire::VotingErrorView` for hosts.
- Schema version 20: `round_immediate_share` holds the round's immediate
  helper-share designation as a row of its own, written once in the same
  transaction as the designated vote's helper plan, immutable, and voided
  only with the undispatched generation it was made for. Version-19
  databases adopt the marker their persisted plans carried. The planner and
  helper-plan preparation read the row and never re-derive a designation
  once it exists, so a designated proposal that leaves the roster, or a lower
  choice recorded afterwards, cannot move it or name a second share.
- `VotingDb::open_wallet_sidecar` shares one connection per sidecar path and
  returns `Arc<VotingDb>`; `VotingDb::scoped` reads another wallet through the
  same connection; `share::pending_rounds_for_accounts` lists pending share
  rounds for several wallets.
- `vote::recover_vote_commitment`, `prepare_vote_work`, and
  `persist_prepared_vote_work` choose the singleton or atomic shape
  internally. `ConfirmedVote`, produced only by `CommittedVote::confirmed`,
  owns helper-share submission.
- Wire views for hosts: typed `NextStepKind`, `RoundPlanActionKind`,
  recovery-work kinds, `WorkflowPhaseView`, chain outcome and failure views,
  round step outcome, failure, and progress views, and `PendingShareRoundView`.

### Changed

- A PIR request now splits one 60-second budget across two bounded attempts
  instead of spending it on a single deadline. A fetch that succeeds does so in
  a few seconds; one against a dead endpoint stalls and never returns, so a
  single long deadline spent its whole budget discovering what a few seconds
  already showed. A shorter deadline is not the fix, though: a wall clock
  cannot tell a stalled endpoint from a slow one, and cutting every request
  short turns a large tier over a congested link from late into unreachable,
  because each attempt restarts the transfer from zero. Each attempt is
  therefore bounded on two axes — connection setup, where a dead endpoint
  reveals itself, and the request as a whole, where a slow link needs room.
  The first attempt gets 5 seconds of connection setup and 15 seconds overall;
  the second gets 10 seconds and the remainder of the budget. A healthy fetch
  never reaches the second attempt, a blackholed endpoint costs 5 seconds
  rather than a minute, and a link that merely needs 40 seconds still finishes.
  Only running out of time is retried, whether the whole-request backstop or
  the connect deadline; a refused connection, a protocol error, or a
  non-success status is a definite answer and is returned as before. Retrying
  is safe for PIR in a way it is not for helper or chain POSTs: the query is an
  idempotent read, and re-sending the identical encrypted query reveals nothing
  — which item is fetched is what PIR hides. The dispatch classification is
  unchanged; PIR acts on it rather than weakening it. Endpoint failover is
  unchanged and still moves on only after the budget is spent, which matters
  most for a deployment publishing a single endpoint, where there is nowhere to
  fail over to.

- **Breaking:** `RouteRequest` gains `connect_timeout: Option<Duration>`, an
  optional bound on connection setup alone, so a caller can abandon an endpoint
  that never answers without also cutting short one that is merely slow. It
  only ever tightens the connect deadline, so connection setup still gives up
  before the whole-request backstop and a stalled connect stays a definite
  pre-dispatch failure. Only PIR supplies one; helper, chain, and tree-sync
  requests pass `None` and are unchanged.

  Only an executor observes connection setup, so `RouteHttp` gains
  `enforces_connect_timeout`, defaulting to `false`, by which an executor
  declares that it bounds setup by that budget. The SDK reads a pre-dispatch
  failure at or after the budget as the budget expiring only from an executor
  that declares it; one that ignores the budget has its pre-dispatch failures
  read as the definite answers they are, so a refusal that merely arrived late
  is never reinterpreted as a timeout and repeated. This mirrors the existing
  `hook_precedes_connection_setup`: a classification the SDK cannot verify is
  drawn only from an executor that declares the property it rests on. A host
  executor that does nothing keeps today's behavior and forfeits only the early
  abandon.

  The declaration follows how an executor was built, not its type.
  `DirectRoute` declares enforcement only when constructed by `new` or
  `with_http_connector`, which wrap the connector in the connection-setup
  deadline. `DirectRoute::with_connector` and `HyperTransport::with_connector`
  take a fully configured connector as given and add no deadline, so a route
  built that way declares nothing: `connect_timeout` does not apply to it, and
  its pre-dispatch failures stay the definite answers they are.

  Enforcement covers connector readiness as well as the connection attempt, so
  a connector that withholds readiness past the deadline reports a connect
  timeout rather than whatever it would have said next. Readiness is bounded by
  racing the connector against a timer that registers the caller's waker rather
  than by sampling the clock: a connector that returns pending decides when it
  is polled again, so a clock read on entry may never run after the deadline and
  the stall would fall through to the looser whole-request backstop. A `connect_timeout` too
  large to represent as an instant leaves the derived deadline unchanged rather
  than panicking.

- **Breaking:** `RoundExecutor::advance_next` is removed. `RoundDriver` chooses
  what to run from a plan it read itself, so a second way to advance a round
  from its plan head is a second driver; hosts drive a round with the driver
  and reach one obligation with `advance_step`.
- **Breaking:** `RoundExecutor::advance_step` refuses a step that the plan it
  reads under the lock still lists but that resolves to no obligation, failing
  with `RoundStepFailureKind::InvariantViolation` instead of returning
  `NoWork`. A step the plan no longer lists is still `NoWork`. The two facts
  come from one read and can only disagree if projection and classification
  have; answering `NoWork` let any host that re-selects from a refreshed plan
  loop on that step forever, so each host had to guard it itself.
- **Breaking:** `RoundStepFailureKind` (and its wire view) gains
  `InsufficientEligibility` and `NoSpendableNotes` so hosts can tell an
  eligibility problem from malformed input without parsing messages.
- **Breaking:** `VotingError::InsufficientEligibility::required_notes` and
  the matching view field are renamed `bundle_note_slots`; the value is the
  bundle's note capacity, not a required note count.
- `session::resume_plan` reads the round once, as one snapshot taken in a
  single deferred read transaction, and derives the plan from that snapshot
  alone. It previously assembled the plan from more than a dozen separate
  reads, so a concurrent write (a tracking pass, an intent write from
  another handle, a chain confirmation) could land between two of them and
  the plan could describe a round state that never existed. The plan is now
  derived by one classifier over that snapshot: votes are grouped into the
  units the chain dispatches (a singleton or one atomic batch), each unit is
  placed by its lifecycle position, roster relation and ballot relation, and
  the resulting obligations are projected into `next_steps` and the plan's
  flags. The rule is specified in `docs/round_orchestration_invariants.md`.
- `RoundExecutor` dispatches on the obligation a fresh plan resolves the
  requested step to, under the round or bundle lock, instead of
  reinterpreting the step: a `CastVote` runs the bundle's whole draft set
  from that obligation rather than rescanning the plan for sibling steps, an
  `AdvanceVoteBatch` recovers the members the obligation names rather than
  re-deriving them from its anchor, and a `ConfirmShare` for a share no
  helper accepted runs delivery from the obligation's own state. Each step
  captures its wallet, round, roster, network, hotkey material and entry
  epoch once, and records its chain outcome and delivery reports in one
  ledger that every outcome, cancellation and failure is built from, so a
  later error cannot drop an earlier confirmation or an accepted delivery.
  Fresh casts and resumed units complete through one path.
- `RoundExecutor::plan` revalidates the stored round's network on every call,
  and `CastVote` checks that the bound hotkey is the bundle's confirmed
  delegation target before the first tree request.
- The wallet example builds one `HyperTransport` for helpers, the chain, and
  the vote tree.
- **Breaking:** the wallet example's `advance_round_until_idle` runs over
  `RoundDriver` instead of its own `advance_next` loop. It takes a
  `RoundDrivePolicy` and returns a `RoundRunReport`, so a caller reads why the
  run stopped rather than inferring it from the last outcome's disposition, and
  `RoundAdvanceError` no longer carries a step failure: the driver isolates
  failures and keeps them, with their durable effects, in the report.

- `advance_until_terminal` escalates to the exact tree immediately after a
  `Recovering` result; `pending_repoll` paces only `Tracking` polls.
- `RoundExecutor::with_binding` validates every roster entry's proposal id
  and option count.
- `PirFleet::new` also normalizes unreserved percent escapes in endpoint
  paths before dropping duplicates.
- The wallet example's `advance_round_until_idle` obtains a fresh host
  context before each step and continues past a raced `NoWork` whose plan
  still lists steps.

- A plan refresh that fails after a step or recovery pass produced a chain
  outcome keeps that outcome on the failure.
- `RoundExecutor::with_binding` rejects a hotkey secret that does not
  reconstruct, before any tree sync.
- `PirFleet::new` canonicalizes endpoint URLs (scheme and host case, default
  port, trailing slashes) before dropping duplicates.

- Delegation proof coordination keys its process-local lock by sidecar
  connection as well as wallet, round, and bundle.
- `CastVote` canonicalizes vote-tree node URLs: trailing slashes are removed
  and a query or fragment is rejected, since the tree client appends its API
  path to the base verbatim.

- The in-memory vote-tree cache prunes entries whose sidecar connection has
  been dropped, so reopening a sidecar does not accumulate retained trees.
- `AdvanceDelegation` keeps the bundle lock inside its re-signing task, so an
  aborted step cannot let a new pass prompt the host signer concurrently.
- Round and bundle locks are keyed by sidecar connection as well as wallet
  and round, so two sidecars sharing a wallet id do not serialize.

- The wallet example's `advance_round_until_idle` takes a `RouteHttp`
  executor and routes helper, vote-chain, and vote-tree traffic through it.
- The wallet example's `advance_round_until_idle` takes the caller's
  `HelperHealth`, so helper failures and cooldowns observed in one call steer
  helper selection in the next instead of resetting per call.
- `VotingDb::open_wallet_sidecar` refuses an empty wallet id with
  `InvalidInput` before opening the sidecar.
- Chain-submission coordination (in-flight, identity, bundle, and round
  locks) is held per open sidecar file, not per SQLite connection, so two
  `VotingDb::open` calls on one path cannot mistake each other's live
  `Submitting` reservation for an abandoned one and dispatch twice.
- The process keeps one vote-tree client per wallet and transport instead
  of one per wallet. A sync over another transport no longer replaces the
  wallet's client and discards its synced state; `sync_vote_tree`,
  `van_witness`, and a round-scoped `reset_vote_tree` are served by the
  client that holds the round, so the standalone sync-then-witness path lands
  on the same state even when another executor synced in between. A routed
  client is kept while a caller holds its transport or while it holds any
  round's tree state, so a host that moves its only transport clone into
  `sync_vote_tree_with` still gets that sync's state from `van_witness`.
- PIR endpoint canonicalization resolves `.` and `..` path segments, so
  `https://pir.example/a/../api` and `https://pir.example/api` dedupe to one
  fleet member and failover never repeats a request against the same
  resource.
- `DelegationPipeline::new` refuses anchor tree state bytes that do not
  decode with `InvalidInput`; a decode failure was previously reported as
  `Internal` and, through the executor, as an invariant violation.
- `RoundStepFailure` carries `share_deliveries`, the
  helper delivery reports accumulated before the failure, so a
  `HelperDeliveryIncomplete` failure or a later error keeps the accepted,
  ambiguous, and pending share results. `RoundStepFailureView` gains the
  matching optional `share_deliveries` field.
- A sidecar's identity is now its file, not its connection: every
  `VotingDb::open` of one path shares one sidecar id within the process, so
  delegation-proof single-flighting, round locks, and vote-tree caches keyed
  by it coordinate across separately opened handles. In-memory databases keep
  distinct ids.
- `VotingDb::scoped` returns `Result` and refuses an empty wallet id with
  `InvalidInput` instead of building a handle whose first wallet-scoped call
  panics.
- Step and recovery failures raised by the final plan refresh (after a step
  advanced, was cancelled, or a recovery pass completed) also carry the
  accumulated helper delivery reports.
- SQLite failures while writing ballot intents or clearing stale share
  delegations are reported as `Storage` (or `DbBusy`) instead of `Internal`.
- `VotingErrorView` carries `setup_field` for `SetupAlreadyPersisted`, so a
  wire consumer can tell a reusable sighash or effects conflict from a fatal
  padded-note-secrets conflict without parsing the message.
- A wallet's vote-tree cache entry lives while any connection to its
  sidecar file is open, not only the handle that populated it, so a second
  `VotingDb::open` handle on the same file still finds a round the first one
  synced after the first is dropped.
- A ballot intent for a proposal that left the roster after its vote reached
  the chain lifecycle (submitted, managed, hashless, rejected, or confirmed)
  no longer withholds `CastVote` or fails helper-plan derivation, and is
  omitted from `RoundPlan::unrostered_intents`: it cannot be cleared, so the
  host could not otherwise unblock the round.
- `VotingDb::set_ballot_intents` takes the expected network and checks the
  stored round's network inside its write transaction, so a mismatch writes
  nothing.
- A sidecar's identity canonicalizes the full path when the file exists, so a
  symlink to a sidecar and its real path share one sidecar id.
- The wallet example's `advance_round_until_idle` gains `routed_pir_fleet` for
  building the host's PIR fleet over the same route. It no longer returns step
  failures through `RoundAdvanceError`: the driver isolates a failed step and
  keeps it, with everything that step had already done, in
  `RoundRunReport::failures`.
- `VotingDb::ensure_round` refuses an existing round whose stored parameters
  (snapshot height, election key, roots) differ from the supplied ones, not
  only one stored for another network, so bundles are never set up under
  parameters a later witness cannot validate.
- Closing the last connection to a sidecar and reopening the same path starts
  a new open span: vote-tree cache entries from before the close are dropped
  instead of being inherited by the reopened, possibly replaced, file.
- A custom `RouteHttp` executor that called the dispatch hook and then
  reported a `BeforeDispatch` failure is now classified as possibly
  dispatched, per the hook contract; only an executor that declares the new
  `RouteHttp::hook_precedes_connection_setup` (the SDK's `DirectRoute`) has
  that phase honored after the hook.
- When helper-share delivery fails partway through a vote, the executor's
  failure and progress reporter now carry the report over the sibling shares
  that were already accepted or ambiguously dispatched, with the failed and
  unattempted shares pending.
- The planner's advancement pass now schedules `AdvanceVote` /
  `AdvanceVoteBatch` for a submitted or managed vote whose proposal left the
  roster while its intent survives, so the on-chain submission is driven to
  resolution instead of being stranded.
- `CastVote` is refused with the new `RoundStepFailureKind::VoteEnded` (wire:
  `vote_ended`) when the host's clock is at or past the authenticated vote
  end, before any tree I/O; advancement and recovery steps are unaffected.
- The wallet example's `advance_round_until_idle` returns the final step's
  own `Advanced` outcome when it leaves the plan idle instead of polling once
  more and returning an empty `NoWork`.
- A committed but undispatched vote for a proposal that left the roster no
  longer holds its bundle: the planner does not count it as a pending vote
  chain, and `CastVote` retires it (new
  `VotingDb::retire_undispatched_votes_outside_roster`) before any tree I/O,
  so a bundle whose sibling vote confirmed is not stranded.
- A vote-tree client is kept in the registry while any caller holds it, so a
  sync in flight cannot be evicted before it creates its round state.
- When a member of a committed, undispatched atomic batch leaves the roster,
  the planner retires the whole batch: no `AdvanceVoteBatch` is planned for
  it and its rostered members are cast again after `CastVote` clears it.
- Planner reads of ballot intents and delegation and share phases classify
  SQLite failures through `from_sqlite`, so external lock contention surfaces
  as `Busy` (`DbBusy`) rather than an invariant violation.
- The planner schedules `SubmitShares` for a confirmed vote whose proposal
  left the roster, so its missing helper shares are still delivered.
- `RoundExecutor::advance_step` runs a `ConfirmShare` step whose share no
  helper has accepted yet as share delivery from the durable plan instead of
  polling for a confirmation no helper can give.
- Helper-plan derivation keeps a persisted round-immediate designation when
  the designated proposal leaves the roster after its vote reached the chain
  lifecycle, instead of recomputing it from the reduced roster and rejecting
  the plans that carry it.
- `RoundPlan::immediate_share_key` reports the round's durable designation
  whenever one exists, so it matches what delivery executes after a roster
  change; it is derived from the ballot only while no designation exists.
- Resumed vote work reconciles the chain before any helper-plan preparation;
  plans are loaded or created after confirmation, right before delivery, so
  an open ballot no longer blocks polling or recovery of a dispatched vote.
  A fresh `CastVote` still prepares its plans before the broadcast.
- `RoundExecutor::advance` validates the recovery request's round id up
  front; a malformed id is `InvalidInput` instead of an idle `NoWork`.
- Retiring undispatched votes propagates a storage failure from the
  lifecycle-ownership check instead of treating it as lifecycle-owned.
- `DelegationPipeline` checks a persisted delegation setup against the
  pipeline's notes and target-bound hotkey before reusing it for signing, the
  same check a persisted proof already gets.
- `RoundExecutor::advance` rejects a round the wallet stores under a network
  other than the chain client's before helper preflight or any plan write.
- A caller queued for a round lock stops waiting when the host moves to
  another operation epoch, not only on cancellation.
- `VotingDb::clear_ballot_intent` evaluates the vote phase inside its write
  transaction.
- `wire::VotingErrorView` accepts unknown fields so the `Other` category
  fallback also survives new structured fields. Unknown `setup_field` values
  decode as `DelegationSetupFieldView::Other`, preserving the error payload
  without treating an unfamiliar setup conflict as reusable.
- The in-memory vote-tree cache is keyed by sidecar connection as well as
  wallet id, so two sidecars sharing a wallet id keep separate tree state and
  transports.

- A failed vote-tree sync resets the round's cached tree inside the detached
  blocking work, holding the round lock, so cleanup happens even when the
  step future is dropped mid-sync.
- The recovery driver keeps the chain confirmation on a failure raised during
  helper delivery, as the step API does.

- `RoundExecutor` proving threads keep the step's round or bundle lock until
  they finish persisting, so dropping a step future mid-proof cannot let a
  new pass start a competing proof.
- `RoundExecutor::with_binding` also rejects a binding whose network differs
  from the network the wallet already stores the round under.
- A helper-delivery error raised after the chain confirmed a vote keeps the
  confirmation in `RoundStepFailure::chain_outcome`.
- `PirFleet` fails over only on retryable `PirUnavailable` errors; local
  `Busy` and `DbBusy` contention is returned to the caller for an
  operation-level retry instead of being repeated against every endpoint.

- `ChainSubmissionClient` captures its wallet at construction and works on
  a private scoped handle; `ChainSubmissionClient::wallet_id` reports it. A
  host re-scoping the handle it passed no longer moves a later pass of an
  in-flight episode to another wallet.
- `RoundExecutor::advance_next` captures the operation epoch before planning.
- `VoteTreeSync::cached_rounds` and `precompute::cached_vote_tree_rounds`
  report which rounds hold in-memory tree state; they replace a test-only
  probe.

- `VotingDb::clear_ballot_intent` decides from the canonical vote phase: an
  intent whose vote the chain lifecycle owns or has finished cannot be
  cleared, and a signed but undispatched vote has its recovery invalidated.
- `CastVote` validates the complete vote-tree node list before any sync:
  every URL must be an http or https URL with a host, and on Mainnet every
  URL must use HTTPS.

- Every bounded chain pass started by `advance_until_terminal_in_epoch`, and
  by the round executor's recovery driver, captures its operation under the
  caller's entry epoch, so an epoch change between the caller's check and
  the pass is refused by the coordinator instead of being adopted.
- **Breaking:** `DelegationPipeline::voting_db` returns a fresh wallet-scoped
  `Arc<VotingDb>` instead of a reference to the pipeline's handle.
- **Breaking:** `DelegationDriver` gains `delegation_target`; the executor
  refuses a driver whose hotkey target differs from the one derived from
  `RoundBinding::hotkey_secret` before proving.
- `RoundPlan::unrostered_intents` (and its wire view) lists durable ballot
  intents for proposals outside the authenticated roster; `CastVote` is
  withheld while any exist, and `VotingDb::clear_ballot_intent` removes one.
- The `RouteHttp` contract now states that implementations must not follow
  redirects.

- **Breaking:** `RoundExecutor::database` returns a fresh wallet-scoped
  `Arc<VotingDb>` over the executor's connection instead of a reference to
  its internal handle, so re-scoping the returned handle cannot move a
  running step's persistence to another wallet.
- **Breaking:** `DelegationDriver` gains `network`; `RoundExecutor` refuses a
  driver whose network differs from its binding before proving.
- `ChainSubmissionClient::advance_until_terminal_in_epoch` runs an episode
  that belongs to work begun earlier under a given epoch; the round executor
  uses it so an epoch change during proving or re-signing ends the step as
  `Cancelled` instead of being recaptured by the chain episode.
- Reusing a persisted delegation proof through `DelegationPipeline` first
  validates the bundle notes and the target-bound hotkey
  (`PreparedDelegationBundle::validate_persisted_proof`), without touching
  PIR.

- `ChainSubmissionClient::advance_until_terminal` and the persisted-vote
  recovery driver `RoundExecutor::advance` capture the operation epoch on
  entry; an epoch change is observed like cancellation between passes,
  during the repoll wait, and at every recovery boundary.
- `RoundExecutor::with_binding` rejects a binding whose network differs from
  the chain client's before any proof or helper-plan work can run.
- A `CastVote` whose tree sync fails after the host cancelled returns a
  `Cancelled` outcome instead of the transport failure the cancellation
  produced; the poisoned tree is still reset first.
- The wallet example's `advance_round_until_idle` returns the last
  `RoundStepOutcome` so a terminal chain diagnostic is not lost with the plan.

- `HyperTransport` derives one absolute deadline per request before polling
  the route; the direct route abandons connection setup a bounded lead ahead
  of that backstop, so a stalled TCP or TLS connect is reported as a definite
  pre-dispatch failure instead of an ambiguous timeout.
- `CastVote` syncs, resets, and generates the VAN witness on one retained
  tree handle, so another executor rebinding the wallet's tree transport
  between those calls cannot hand the witness a client missing the synced
  round state.

- `RoundExecutor` steps capture the host's operation epoch on entry and treat
  an epoch change like cancellation at every boundary where a step decides to
  continue, including before helper preflight; a session or account switch
  can no longer dispatch a vote or helper share from a stale invocation.
- Every `DelegationPipeline` stage verifies that the pipeline's own database
  handle still selects the wallet captured at construction and fails with
  `InvalidInput` otherwise.

- **Breaking:** `DelegationDriver` gains `wallet_id` and
  `shares_database_with`. `RoundExecutor` refuses a driver whose wallet or
  sidecar connection differs from its own frozen scope before invoking any
  delegation stage. `DelegationPipeline` captures its wallet at construction
  the same way and exposes it through `wallet_id`.
- `RoundExecutor::with_binding` rejects an empty or repeated proposal roster
  with `InvalidInput`; an empty roster previously planned as vacuously
  decided and skipped the round.
- `VotingDb::open_wallet_sidecar` keys a bare relative file name through the
  current directory, so `wallet.db`, `./wallet.db`, and the absolute path share
  one connection.
- A PIR URL the transport cannot build is classified as a non-retryable
  `PirHttpFailurePhase::Build` failure instead of a retryable connect error.

- `wire::VotingErrorKindView::Other` is a Serde catch-all: an error category
  added by a newer crate deserializes into it instead of failing an older
  host's `VotingErrorView` parse.
- PIR connect failures that carry a typed `PirHttpFailure` keep its
  retryability even when the response body echoes a layout or poly_len
  mismatch message, so `PirFleet` still fails over on a retryable status.
  Text matching applies only to errors without typed transport metadata.

- `ChainSubmissionClient::advance_until_terminal` observes cancellation
  during the `pending_repoll` wait between passes, within 25 ms, instead of
  only at the start of the next pass.

- A `ChainSubmissionControl` now wakes whatever is waiting on it the moment it
  is cancelled or re-epoched, instead of being re-read on a 25 ms or 50 ms
  tick. Both waits that honour it — the `pending_repoll` wait between chain
  passes and the round driver's repoll wait — race their delay against the
  control, so an interruption is observed at once and an uninterrupted wait
  costs one timer however long it lasts. Polling was affordable only while
  every wait was short; a wait that spans the hours until a delayed helper
  share is due would otherwise wake tens of times a second for its whole
  duration.

- `VotingDb::open_wallet_sidecar` serializes concurrent opens per sidecar
  path only. A slow open of one sidecar (busy timeout, migrations, busy
  retries) no longer delays opening a different sidecar.

- `CastVote` drops the round's cached vote tree after every failed node
  sync, including the last node's and a cancelled attempt's, so a partially
  appended or root-mismatched tree cannot poison the next node or the next
  pass.

- `RoundExecutor::advance_step` rejects a step whose bundle still has a
  `Delegate`, `AdvanceDelegation`, or `AdvanceImportedDelegation` step ahead
  of it in the plan with `InvalidInput`, before any lock-scoped work or
  network I/O. `advance_next` is unaffected: it always runs the plan head.

- `RoundExecutor` freezes the wallet it is constructed for: it works on its
  own handle over the shared sidecar connection, so a host `set_wallet_id`
  cannot retarget a waiting or running step, and every operation fails with
  `InvalidInput` if the executor's own handle is re-scoped.

- A host-provided Keystone signature (`KeystoneSignatureSource::Provided`) is
  persisted under its bundle as soon as the signed payload verifies, so a
  `Delegate` step cancelled before chain dispatch, or a restart, resumes
  through `KeystoneSignatureSource::Stored` without asking the device to sign
  again. A cancelled `Delegate` outcome now also carries the signed bundle in
  `RoundStepOutcome::delegation`.

- **Breaking:** `NextStepView.kind`, `RoundPlanView.primary_action`, the
  recovery-work `kind` fields, and every wire `phase` field are enums instead
  of strings. Serde labels are unchanged. The crate-side `as_str` and
  `NextStep::kind` string tables are removed.
- **Breaking:** `VotingDb::open_wallet_sidecar` returns `Arc<VotingDb>`.
- **Breaking:** functions that took `&PirClientBlocking` take
  `&dyn PirProofSource`; existing callers coerce.
- **Breaking:** `HyperTransport` is `HyperTransport<R: RouteHttp = DirectRoute>`.
  `new`, `with_http_connector`, and `with_connector` keep their shapes.
- Transaction begin and commit failures classify SQLite busy and locked
  errors as `VotingError::DbBusy`.
- Eligibility, no-spendable-note, write-once setup, and PIR failures carry
  structured fields. Their display text keeps the earlier wording.

### Fixed

- Three guards that must refuse an action on a vote already on chain now
  recognise one confirmed by an exact-tree scan. `ensure_vote_rebuild_allowed`,
  `ensure_no_submitted_vote_conflict_for_intent` and `store_vote` each asked
  whether `votes.tx_hash` was set — a hash that exists only for a hash-confirmed
  submission, because the schema requires `confirmation_source = 'tree'` to
  carry none. So a tree-confirmed vote could be rebuilt into a competing
  generation, a ballot intent disagreeing with it was accepted, and its choice
  and commitment could be overwritten outright, all silently: unlike the
  completion checks above, these failed open, permitting what they exist to
  refuse rather than blocking what should proceed. Each now accepts either
  witness. A test scanning for the shape all of these shared guards against the
  next one.

- A vote chain is judged complete by its commitment-tree position rather than
  its transaction hash, and a proposal-authority bit clears on either witness.
  Both checks read `votes.tx_hash`, which exists only for hash-confirmed
  submissions: the schema requires `confirmation_source = 'tree'` to carry none.
  A vote confirmed by an exact-tree scan therefore looked unfinished forever —
  it blocked every later proposal on its bundle with a message asking the caller
  to confirm a vote already confirmed, and its authority bit stayed set so the
  next vote rebuilt a stale vote-authority note and the chain rejected the
  nullifier as already spent. Neither could be recovered from, because the
  missing hash does not exist. The tree position is written by confirmation on
  both routes and cleared when a generation is invalidated, so it tracks
  completion; the hash still counts a submitted vote whose transaction has
  landed but is not yet confirmed locally.

- A vote whose POST was dispatched but never classified is now treated as
  having spent its delegation VAN. `load_van_tree_entries` skips the VAN-leaf
  expectation once a bundle has voted, because casting spends that VAN, but it
  read only `votes.tx_hash`. A dispatched-and-unclassified vote has no hash yet
  may already be on chain, so the bundle was still expected to hold its original
  `gov_comm`, the witness failed to verify, and tree sync failed with "confirmed
  delegation bundle N does not match its synced vote-tree leaf". The failure was
  self-sustaining: tree sync failing aborted the cast that would have recorded
  the hash, so the next pass failed identically, and one bundle's stale
  expectation aborted the sync for every other bundle too. A committed
  `chain_submissions` reservation is now consulted as well — the same evidence
  the submission lifecycle uses to refuse a second POST, so the tree expectation
  and the lifecycle no longer disagree about whether a transaction may exist.
  A terminally `rejected` row is excluded, because it spent nothing and, being
  terminal, can never acquire an outstanding POST afterwards; counting it would
  retire the expectation for the rest of the round and silently skip the
  delegation-leaf check. A `recovering` row carrying the same rejection is not
  excluded: exact-tree recovery can reserve a retry and release a POST from it
  without changing either column, so it cannot be told apart from a quiescent
  rejection and must be read as possibly-spent.

- The helper-attempt marker is now recorded in release builds. The reservation
  performed its only state mutation inside `debug_assert!(state.begin(&url)?)`,
  and `debug_assert!` compiles its argument out of a release build — so release
  wrote `attempting_urls = []`, still saw its compare-and-swap succeed, and
  returned `Started` for a reservation it had not made. A POST must be durably
  journaled before dispatch so an interrupted attempt stays recoverable: on
  restart the helper is known to have been tried with an unknown outcome, and is
  polled rather than re-sent blindly or written off as never contacted. Release
  builds wrote no such evidence, so a crash mid-POST left an attempt
  indistinguishable from one never made. Tests never caught it because they run
  with debug assertions enabled and therefore execute the call release drops;
  it surfaced only by crashing a release binary mid-POST and reopening the
  sidecar in another process.

- A bundle can persist its delegation proof after another bundle has already
  advanced the round-wide display phase to `VoteReady`. The proof transaction
  wrote `DelegationProved` through `advance_round_phase`, which refuses a
  regression by returning an error rather than moving the marker backwards.
  That error aborted the enclosing immediate transaction, so the proof and
  proof-result rows were rolled back with it and a valid multi-bundle recovery
  lost work it had already completed. `rounds.phase` is a lossy round-wide
  high-water mark, not an authority for per-bundle completion, so the write is
  now a `queries::advance_round_phase_to_at_least` that leaves a later marker
  alone and commits the bundle-local artifacts either way. The delegation
  capability import, which open-coded the same rule, shares that helper.

- Schema version 21 rebuilds `chain_submissions` onto the 50-proposal bound.
  A sidecar migrated by a build that carried the 15-proposal bound kept that
  CHECK at version 20; nothing rewrote it, and the version-20 fingerprint
  check then refused to open the sidecar at all, so every voting call on that
  wallet failed with an unsupported-chain-submission-schema error. Rows are
  preserved, and the rebuild is a no-op for a database that already holds the
  widened bound.

- `VotingDb::delete_round` refuses a round whose delegation has reached the
  network, since its stored setup is the only thing that can reproduce that
  round's voting weight. `VotingDb::delete_round_discarding_recovery` is the
  explicitly named escape hatch for abandoning such a round on purpose, and is
  what the corrected-capability-package reset uses. Both take the round's
  chain-submission gate before reading the evidence they act on, and the
  checked path re-reads that evidence inside the transaction that deletes, so a
  submission starting concurrently cannot lose the rows that recover it.

- Delegation setup that replaces an existing binding is refused once its
  bundle shows broadcast evidence, and the check runs in the transaction that
  writes. The hotkey rebuild holds the round's submission gate from the discard
  through the replacement write, so a lifecycle call cannot dispatch the old
  setup in between and have the replacement written over it. A first write, and
  an idempotent rewrite of the same binding, are unaffected.

- **Breaking:** pruning a bundle suffix that shows broadcast evidence fails
  with `DelegationAlreadyBroadcast` instead of `Busy`. The evidence is durable,
  so the old classification told a host retry loop to repeat a call that can
  never succeed. `VotingErrorView` now carries `bundle_index` for both
  `DelegationTargetMismatch` and `DelegationAlreadyBroadcast`.

- A chain rejection's diagnostic carries the node's own explanation instead of
  a bare numeric code, escaped and bounded as before. Non-JSON responses report
  their content type and body the same way.

### Removed

- **Breaking:** removed `share::pending_rounds` and `share::PendingShareRound`.
  `share::pending_rounds_for_accounts` returns the same rounds with the wallet
  each belongs to, which is what a multi-account host needs to act on one.
- **Breaking:** removed `share::next_tracking_delay_for_round`,
  `share::policy::next_tracking_delay_seconds` and
  `share::policy::is_share_ready_for_status_check` from the public API, along
  with the four constants only they read:
  `SHARE_STATUS_CHECK_GRACE_SECONDS`, `SHARE_READY_POLL_INTERVAL_SECONDS`,
  `SHARE_FUTURE_CHECK_MAX_DELAY_SECONDS`, and
  `SHARE_MIN_TRACKING_DELAY_SECONDS`. Pass cadence is `ShareTrackingDriver`'s:
  it repeats a pass on the delay each pass computes, shortens that delay to
  what is left before vote end — which the pass cannot do, since it computes
  from share rows alone — and reports why it stopped through
  `ShareTrackingQuiescence`. A host that scheduled passes itself should run the
  driver and observe its events; one that needs a default value reads it from
  `ShareTimingPolicy::default()`, which is still public and is what the
  driver's policy is built from. Per-share readiness is still readable through
  `share_tracking_flags`, whose `ready_for_status_check` is exactly the retired
  predicate.
- **Breaking:** removed `RoundExecutor::advance_step` and
  `ChainSubmissionClient::advance_until_terminal`. `RoundDriver` is the
  supported way to run a round's steps: it re-plans, orders bundle-locked work,
  and carries the operation epoch it dispatched under into each step, which a
  host selecting one step at a time cannot do. A host that ran steps itself
  should call `RoundDriver::run`.

- **Breaking:** removed the persisted-vote recovery driver
  `VoteRecoveryExecutor::advance` with `VoteRecoveryRequest`,
  `VoteRecoveryAdvance`, `VoteRecoveryDisposition`, `VoteRecoveryFailure`,
  `VoteRecoveryFailureKind`, `VoteRecoveryProgress`,
  `VoteRecoveryProgressReporter`, `VoteRecoveryProgressBridge`,
  `NoopVoteRecoveryProgressReporter`, and the `VoteRecoveryExecutor` alias.
  It duplicated the step path with a second failure ladder; `RoundDriver`
  resumes persisted vote work through the ordinary step path.
  `VoteRecoveryKey` and `VoteShareDeliveryReport` remain as the identity a
  step's progress and delivery reports carry.
- **Breaking:** removed `VotingDb::build_and_prove_delegation` so durable
  delegation proofs cannot bypass process-local single-flight coordination.
  Use `delegate::ensure_proof` or
  `PreparedDelegationBundle::ensure_proof`; both validate the supplied notes
  and target-bound keys before returning a generated or reused proof. Proof
  progress is delivered live from a delivery thread the producer never waits
  on, so reporters may reenter or dispatch proof work without deadlocking, and
  terminally rejected submissions retain their generation-bound proof.
- **Breaking:** removed `CommittedVote::submit_prepared_shares`; submit
  through `ConfirmedVote::submit_prepared_shares`.
- **Breaking:** removed `delegate::DelegationSigner` and replaced
  `AdvanceDelegation::signer` with `spend_auth_signature`. Delegation chain
  submission now accepts only the external SpendAuth signature and loads the
  authoritative PCZT sighash and randomized verification key from durable SDK
  state.
- **Breaking:** collapsed the session planner's submit/poll step duality now
  that one bounded `advance_*` call both dispatches and reconciles.
  `NextStep::{SubmitVote, PollVote}` become `AdvanceVote`,
  `NextStep::{SubmitVoteBatch, PollVoteBatch}` become `AdvanceVoteBatch`, and
  `NextStep::PollDelegation` becomes `AdvanceDelegation`; `Delegate` still
  means "this bundle needs a signed delegation from the wallet" and stays
  distinct. `VoteRecoveryWorkKind` and `DelegationRecoveryWorkKind` collapse the
  same way. Stable FFI `kind` strings change to `advance_delegation`,
  `advance_vote`, and `advance_vote_batch`.
- **Breaking:** removed the version-17 chain-submission and confirmation
  mutation APIs. `ChainSubmissionClient` is now the only public route to
  submission, polling, recovery, and confirmation. Deleted
  `confirmation::{confirm_delegation_submission, confirm_vote_submission,
  confirm_vote_batch_submission}`, `delegate::{record_submission,
  record_van_position}`, `vote::{record_submission, record_batch_submission,
  record_vc_position}`, and `CommittedVote::{record_submission,
  record_vc_position}`. The `confirmation` module is now private, so the
  host-supplied chain-event vocabulary `TxEvent`/`TxEventAttribute` and the
  `DelegationConfirmation`/`VoteConfirmation`/`VoteBatchConfirmation` result
  types are no longer public; event parsing is a private lifecycle mechanism.
- **Breaking:** removed the chain-ready payload builders `vote::submission`,
  `CommittedVote::submission`, and `delegate::submission`. The lifecycle
  constructs and dispatches the transaction itself.
  `PreparedDelegationBundle::{submission, signed_bundle}` remain for the
  delegation capability-handoff export flow.
- **Breaking:** the raw storage writers behind those APIs are no longer public
  API. `storage::queries::{store_van_position, store_delegation_tx_hash,
  record_vote_submission}`,
  `VotingDb::{store_van_position, store_delegation_tx_hash,
  record_vote_submission, mark_delegation_submitted, mark_vote_submitted}`, and
  `vote::{record_submission, record_batch_submission, record_vc_position}` are
  crate-private test helpers that no Cargo feature, including `test-fixtures`,
  exposes. Read-only projections are unchanged.

### Added

- Added the public bounded `ChainSubmissionClient` for delegation and
  singleton-vote and atomic vote-batch submission, status advancement, and opt-in exact
  commitment-tree recovery. Its internal coordinator and store provide durable
  pre-POST reservation, bounded failover, candidate-first reconciliation,
  restart-stable tracking deadlines, sticky recovery, atomic confirmation,
  canonical lifecycle serialization, store-owned lock authority, causal bundle
  admission, strict confirmation-event validation, unique candidate ownership,
  and exact committed-reservation accounting.
- Added `AdvanceImportedDelegation`,
  `ChainSubmissionClient::advance_imported_delegation`, and the matching
  planner/recovery-work variants for capability imports. The lifecycle lazily
  adopts the stored package hash, polls without a signer, POST, tree scan, or
  retry, confirms atomically, and terminally rejects a committed failure.
- `RoundPlan::delegation_statuses`, `RoundRecoverySnapshot::{delegation,
  votes}`, and their wire views carry `submission_diagnostic`, the diagnostic
  stored on the authoritative lifecycle row. It is always present for the
  terminal `SubmittedWithoutHash` and `SubmissionRejected` phases, which
  schedule no further lifecycle call, so a host can show why manual handling
  is needed after a restart without re-driving the lifecycle.
  `ChainSubmissionDiagnosticKind::as_str` exposes the stable discriminator
  used by storage and the views.
- `RoundPlan` and `RoundPlanView` expose derived work predicates:
  `needs_delegation_signing`, `has_in_flight_delegation`, `needs_vote_polling`,
  `has_remaining_vote_or_share_work`, and `has_recoverable_vote_or_share_work`.
  Hosts should read these instead of matching `NextStepView::kind` strings.
  They are computed from an exhaustive match over `NextStep`, so adding a step
  variant is a compile error in the SDK rather than an unrecognised kind that a
  downstream allowlist silently reads as "no work" — a failure mode that
  strands a round with no symptom.
- `chain_submission` carries `compile_fail` doctests asserting that every
  removed mutation API and raw writer stays off the public surface, satisfying
  the compile-time surface check in `chain_submission_invariants.md`.

### Changed

- **Breaking:** the configured vote-chain id no longer binds a chain-submission
  identity or its generation digest. It selects where a request is dispatched,
  not what the request means, so one identity now covers a wallet's round,
  bundle, and target across every configured vote chain. The version-1
  generation digest vectors change accordingly, and `chain_submissions` drops
  its `vote_chain_id` column and its partial identity indexes in favour of one
  identity key.
- **Breaking:** the `VotePhase::LegacyConfirmed` workflow phase and the
  `legacy_import` / `legacy_projection` confirmation sources are removed, and
  `ChainSubmissionDiagnosticKind` drops `RecoveryUnavailable`,
  `GenerationDerivationFailed`, and `LegacyEvidenceInvalid`. Every
  `chain_submissions` row now carries a non-null generation digest; there is
  no unbound or migration-only row class.
- The version 17 to 18 migration only adds the `chain_submissions` schema.
  Version-17 domain columns are preserved untouched so completed rounds keep
  displaying through the existing domain-column phase projection; no
  version-17 evidence is imported and the lifecycle never owns a pre-upgrade
  submission. Upgrading a database that holds an in-flight version-17
  submission is unsupported.
- **Breaking:** delegation recovery views now expose VAN positions as `u64`,
  matching lifecycle confirmation and SQLite's supported non-negative range.
- Expanded the supported proposal-ID and atomic vote-batch ranges from 1–15 to
  1–50 while retaining 16 encrypted shares per vote commitment. This consumes
  the breaking circuit and verification-key change from `voting-circuits
  0.12.0-rc.1`.
- **Breaking:** `session::resume_plan`, `recovery::round_snapshot`, and
  `VotingDb::{delegation_phase, delegation_phases, vote_phase, vote_phases}`
  now derive submission state from the authoritative `chain_submissions` row
  instead of the version-17 projection columns. A generation that is
  `Submitting`, `Tracking`, or `Recovering` reports as submission-managed and
  yields an advance step, so a transaction that may already be on the wire is never
  dispatched a second time. Those columns record a hash only on confirmation,
  so the previous behavior reported an in-flight transaction as never
  submitted. Poll steps now carry `tx_hash: None` while a reserved generation
  has not yet produced a candidate hash, instead of failing the plan.
- **Breaking:** rejected authoritative rows now project as the distinct
  `SubmissionRejected` phase instead of `SubmissionManaged`, and schedule no
  advance work. `needs_delegation_signing` is now true for local
  `AdvanceDelegation` retries; imported capability polling is represented by
  its signer-free step instead.
- The planner's step-consuming derivations no longer use wildcard match arms, so
  an unclassified `NextStep` cannot be silently dropped inside the SDK either.
  As a result a blocking `ConfirmShare` is now withheld from recovered vote work
  while its vote still has any outstanding chain work, not only while it is
  awaiting confirmation; such a share is not actionable until the vote confirms.

### Fixed

- Exhausting an invocation's POST budget on a possibly-dispatched attempt, a
  colliding hash, or cancellation during the final dispatch no longer ends a
  generation as `SubmittedWithoutHash`. The row stays hashless `Recovering`
  with its last dispatch diagnostic, and a later invocation may scan or retry.
  `SubmittedWithoutHash` is now reachable only through chain rejection code 2
  after unresolved dispatch.
- `ExactTree` advancement of a hashless dispatch-ambiguity row now scans the
  tree before any POST, and a code-2 rejection after unresolved dispatch runs
  one tree pass before the terminal transition, so a generation that already
  landed is confirmed with positions instead of stranding its bundle.
  Status-only advancement keeps the direct retry.
- A non-final exact-tree recovery retry whose accepted hash collides with
  another generation now continues through the remaining bounded attempts.
- `ExactTree` advancement of an imported delegation never scans the tree.
- `VoteRecovery::tx_hash` in the round snapshot now reports the batch row's
  candidate hash for in-flight batch members, matching `VoteRecoveryWork`.
- A usable hash that follows durable dispatch ambiguity clears the stored
  ambiguity diagnostic when the row enters `Tracking`.
- A vote-chain POST that hits the SDK's own deadline before the transport
  marks dispatch is now classified as definitely unsent, so the fresh
  reservation is removed and bounded failover continues instead of persisting
  terminal `SubmittedWithoutHash` for a request that never left the wallet.
- Rejection code 2 on an exact-tree recovery retry ends a generation as
  `SubmittedWithoutHash` only when the durable row still carries unresolved
  dispatch evidence. A retry after a rejected POST or a committed failure now
  surfaces an error and leaves the row recoverable.
- A possibly-dispatched recovery retry now durably records the new dispatch
  ambiguity and continues through the invocation's remaining bounded attempts;
  a later invocation may reserve the next POST directly instead of repeating
  a full tree pass.
- `resume_plan` now schedules `AdvanceVote` and `AdvanceVoteBatch` for
  lifecycle-owned and submitted votes whose proposal has no recorded ballot
  intent, and a missing intent no longer fails planning for a batch member.
- `VoteRecoveryWork::tx_hash` for `AdvanceVoteBatch` now reports the in-flight
  batch row's candidate hash, looked up by ordered batch digest.
- Local delegation, singleton-vote, and vote-batch `resume_plan` advance steps
  now direct hosts through exact-tree recovery. Following the documented
  pending loop can therefore resolve a hashless `Recovering` generation instead
  of repeatedly returning its unchanged status-only result; imported
  delegations remain poll-only.
- Delegation advancement no longer reparses or requires a returned full PCZT
  to reconstruct its sighash. Background-precomputed, recovered, and Keystone
  flows may omit PCZT bytes without stranding submission in the host adapter.
- Chain-submission cancellation now removes a fresh reservation when transport
  dispatch has not begun. Batch admission derives its identity locks from the
  complete request roster, verifies the persisted roster before reading any
  member row, and rejects oversized rosters before lock allocation.
- SQLite chain-submission admission now permits confirmed predecessors to
  advance, refuses a delegation reservation once a confirmed vote or batch
  exists in the bundle, classifies reused candidate hashes as hashless recovery, preserves
  monotonic lifecycle timestamps across wall-clock rollback, and retains
  possible-dispatch evidence when restart normalization cannot be persisted.
- Ballot-intent changes and bundle pruning now preserve every active semantic
  generation and its helper-delivery material under the lifecycle round gate.
- Lifecycle ownership checks now serialize with every compatibility projection
  write, unresolved bundle predecessors remain blocked across vote-chain id
  changes, and tracking diagnostics survive database reopen. Migration rejects
  the earlier unreleased v18 schema by fingerprint; session reset and deletion
  retain bundle-scoped and legacy-round progress.
- Session cleanup now preserves delegation setup fields for bundles with a
  successful proof so wallets can resume signing without regenerating ZKP1.
- VAN positions above `u32::MAX` are now read losslessly; legacy `u32` readers
  return a range error instead of wrapping.

### Removed

- Removed the standalone `recovery::clear` and
  `VotingDb::clear_recovery_state` APIs. Ordinary reset preserves durable
  submission evidence; explicit round or account deletion remains the
  destructive cleanup boundary.

## v3.1.0

### Changed

- Released the exact `v3.1.0-rc.16` implementation as `v3.1.0` without
  implementation changes. Its supporting production snapshots were released
  as `pir-types 0.6.2`, `pir-client 0.7.2`, `voting-circuits 0.11.2`,
  `vote-commitment-tree 0.6.0`, and `vote-commitment-tree-client 0.8.0`.

## v3.1.0-rc.16

### Changed

- Local voting-hotkey delegation now derives each bundle's VAN blinding from
  the stored hotkey secret and exact round and bundle identity. Restoring that
  secret and using `recoverable_bundle_policy_v1()` reconstructs the same VAN
  after voting database loss without new authority-root or recovery tables.

## v3.1.0-rc.15

### Fixed

- Persisted helper-share plans can now resume after the authenticated helper
  fleet changes. Plans remain bound to their original planning fleet and
  target, while removed helpers are not contacted and current helpers are
  eligible as fallbacks.

## v3.1.0-rc.14

### Added
- `VotingDb::store_keystone_signatures_batch` now provides atomic, idempotent
  Keystone signature persistence, and `VotingDb::clear_wallet_state` also
  removes the wallet's round-independent PIR cache.
- `RoundPlan` and `RoundPlanView` now expose `immediate_share_confirmed`.
- Added atomic multi-proposal vote batches through
  `commit_atomic_vote_batch`, `prepare_atomic_vote_batch`, and
  `recover_atomic_vote_batch`. `SignedVoteBatch` provides the canonical
  `cast-vote-batch` request, while restart planning and confirmation preserve
  the batch as one recoverable authority chain. Existing batch APIs remain
  singleton compatibility wrappers.
- Added a typed `HelperClient` and host-owned `HelperTransport` for readiness,
  share submission, and status polling. The client validates protocol data,
  bounds requests and responses, applies endpoint-specific retry rules, and
  tracks helper health. `HyperTransport` remains the default direct transport;
  wallets can inject custom, proxy, or Rustls-wrapped Hyper connectors.
- Added `track_pending_shares` for durable confirmation and recovery of pending
  helper shares, plus `confirm_pending_share` for checking one share with the
  same quorum, timeout, cancellation, and health-ordering rules.
- Added the SDK-owned helper delivery lifecycle:
  `HelperClient::preflight_fleet`,
  `CommittedVote::prepare_share_delivery`, and
  `CommittedVote::submit_prepared_shares`. It persists and resumes one
  generation-bound plan for the complete commitment before submitting shares,
  without exposing encrypted helper payloads to the host.

### Changed
- Helper-share planning, persistence, submission, and recovery are now
  authoritative SDK responsibilities. Hosts provide authenticated helper
  configuration, round timing, transport, and cancellation.
- **Breaking:** invalid helper URLs now fail with
  `VotingError::InvalidInput` before network I/O instead of being silently
  dropped. `helper::url::canonicalize_helper_base_url` and
  `canonical_helper_url_list` are public so hosts can validate configuration.
- **Breaking:** initial helper delivery now uses
  `CommittedVote::prepare_share_delivery` followed by
  `CommittedVote::submit_prepared_shares`; the per-share
  `submit_share_to_helpers(ShareSubmissionRequest)` API was removed.
- **Breaking:** `ShareDeliveryPlanningParams` now accepts the authenticated
  round's complete `proposal_ids` roster and derives the immediate share from
  durable ballot intent.
- **Breaking:** encrypted helper payloads and their low-level construction and
  recovery APIs are no longer public. Vote-chain submission continues to use
  the public `VoteCommitmentWire`.
- **Breaking:** helper confirmation polls the complete configured fleet and
  requires agreement from two distinct helpers when at least two are
  configured. Direct confirmation and confirmation-persistence APIs were
  removed in favor of `track_pending_shares` and `confirm_pending_share`.
- **Breaking:** removed `HELPER_PREFLIGHT_TIMEOUT_SECONDS`; preflight timing is
  now derived from
  `share_policy::SHARE_HELPER_PREFLIGHT_SOFT_TIMEOUT_MILLISECONDS`.
- `HelperClientConfig` now validates nonzero deadlines and permits at most two
  nonzero retry delays. Confirmation polling is limited to four concurrent
  requests and ten seconds per share.
- Schema versions 16 and 17 persist definite, ambiguous, and interrupted
  delivery outcomes together with complete generation-bound helper plans.
  Legacy rows remain readable but do not weaken placement or quota validation.

### Fixed
- Wallet examples now separate vote-chain submission from helper delivery and
  use the preflight, persisted-plan, and prepared-batch APIs.
- Helper plans remain valid across normal vote confirmation while staying
  bound to the exact vote generation, wallet scope, configured fleet, durable
  VC-tree position, and complete payload set. Stale or inconsistent plans fail
  before network or storage side effects.
- Whole-plan validation now rejects duplicate helpers, fleet drift, invalid
  schedules, target-count drift, and aggregate quota violations before the
  first POST. Concurrent submissions share a process-wide 16-request limit and
  cannot overfill placement targets.
- Helper attempts are journaled before dispatch and retain accepted,
  ambiguous, interrupted, or definite-failure outcomes across cancellation and
  restart. Outcome-unknown POSTs are never retried as definitely unsent;
  overdue recovery uses duplicate-safe reconciliation.
- Recovery preserves durable reveal schedules and placement history, waits for
  vote confirmation, replenishes complete deficits, and rechecks confirmation
  and vote-end state before each POST. Legacy or malformed helper identities
  remain readable without participating in delivery.
- Helper requests now enforce shared deadlines, bounded JSON responses, and
  content types even for custom transports. Boundary completions,
  cancellation, retry backoff, and helper-health scoring no longer lose or
  double-count completed outcomes.
- Nullifier-inconsistent recovery bundles are reported as unrecoverable, and
  delayed network results cannot mutate a replacement share generation.
- SQLite operations that validate durable voting state before updating it now
  use immediate transactions, preventing concurrent WAL writers from causing
  stale-snapshot `database is locked` failures during submission and
  confirmation recording.

## v3.1.0-rc.13

### Changed
- `zcash_voting` now defaults to Zakura and exposes upstream librustzcash
  through the mutually exclusive `lrz` feature while depending directly on
  the leak-free `zakura` or `lrz` complete backend mode from
  `zakura-wallet-lib`. This keeps wallet-family selection in one facade while
  preventing disabled Zakura forks from entering LRZ consumers' Cargo
  lockfiles and metadata. See the "Dependency notes" section of
  `zcash_voting/README.md`.
- Updated the Zakura stack to wallet-libraries RC4 and stable crypto 1.0,
  `voting-crypto-deps 0.2.2`, `voting-circuits 0.11.2`, `imt-tree 0.5.2`,
  `pir-types 0.6.2`, and `pir-client 0.7.2`. This raises the workspace MSRV to
  Rust 1.91.
- Prepared `vote-commitment-tree 0.6.0` and
  `vote-commitment-tree-client 0.8.0` for their Zakura-default feature
  contracts; publish them before `zcash_voting 3.1.0-rc.13`.

## v3.1.0-rc.12

### Added
- Added shared progressive helper timing and initial-delivery limits, plus
  readiness-ranked batch planning that balances a commitment's initial shares
  across the preferred helper pool.
- Added process-local `HelperHealth` scoring that demotes repeatedly failing
  helper servers for fixed cooldown windows, immediately re-demotes them on the
  first failure after expiry, and never removes them from candidate lists.
- Added public helper URL canonicalization for stable server identity. Helper
  base URLs may use HTTP or HTTPS and a mount path, but not credentials, query
  parameters, or fragments; equivalent default ports, trailing slashes, and
  mount-path percent escapes are normalized before comparison or persistence.
- Added a host-owned `HelperTransport` abstraction for helper-server requests.
  The bundled `HyperTransport` provides direct HTTP, while wallets can supply
  Tor or proxy-backed transports without fallback to a different route.

### Changed
- Initial share delivery continues to target half the configured fleet, rounded
  up, while balancing a complete commitment across the ready helper pool.
  Retries may exceed the initial distribution for liveness.

## v3.1.0-rc.11

### Added
- `DelegationKeys::with_round_bound_voting_target` is now public, allowing
  callers to bind a secret-free `RoundBoundVotingHotkeyTarget` directly without
  depending on `WalletDb`, lightwalletd, or
  `prepare_delegation_bundle_for_target`.

### Fixed
- `DelegationKeys::with_round_bound_voting_target` now retains the validated
  public target. Lower-level delegation setup, signing request, and proof APIs
  reject those keys when used with a different stored voting round.

## v3.1.0-rc.10

### Added
- Added deterministic round-level immediate-share selection: share index 0 of
  the lowest voted proposal in the lowest-value eligible bundle is designated
  for immediate helper submission. `RoundPlan` and `RoundPlanView` expose the
  selected `ImmediateShareKey`, while batch submission plans mark the matching
  caller-supplied batch position with `immediate = true` and `submit_at = 0`.

## v3.1.0-rc.9

### Added
- `lwd::anchor_tree_state_with_retry_on` fetches the snapshot note-commitment
  tree on a caller-owned lightwalletd client, so a wallet that already holds a
  channel (Tor, a proxy, a pool) keeps that route instead of the crate dialing
  a second, always-direct connection.
- `precompute_snapshot_bundles` persists the canonical bundle plan for a
  snapshot-stable note set, samples padded-note secrets, and warms PIR for
  real notes plus padded-slot nullifiers. The round must already exist; no
  hotkey or wallet DB is required. Once the wallet is scanned through
  `snapshot_height`, historical note selection is frozen, so first-write-wins
  bundle rows are the intended lock-in rather than a stale-plan hazard.
  Witnesses still come from `prepare_delegation_bundle`. New type:
  `SnapshotBundlePrecomputeReport`.

### Changed
- Voting no longer builds a whole `WalletSummary` just to learn how far the
  wallet has scanned. The sync guards behind `select_notes_with_wallet_db` and
  `prepare_delegation_bundle` now read `block_fully_scanned` — one indexed
  `scan_queue` row plus one `blocks` row — and fall back to
  `birthday_height - 1` exactly as the summary does. Everything else the
  summary computed was discarded: Sapling and Orchard scan-progress estimates
  that scan the full `blocks` table with a correlated subquery per row,
  per-account balances joined across all three shielded pools, and a shard-root
  read per pool. That work grows with the size of the wallet, and it ran on
  every note selection and every delegation. The summary also opened a nested
  transaction, which errors outright if two threads ask at the same time.
- **Behaviour:** when the wallet summary was unavailable — no chain tip
  recorded yet, or scan progress not estimable — the sync guard previously read
  a scanned height of 0 and rejected every nonzero snapshot height. It now
  reads the height actually scanned. Voting needs the snapshot height to be
  covered by the scan; it does not need the wallet to know the chain tip.

### Removed
- **Breaking:** URL-taking lightwalletd helpers that opened their own channel:
  `latest_block_height`, `latest_block_height_with_retry`, `tree_state_bytes`,
  `anchor_tree_state_with_retry`, and `anchor_tree_state_bytes_with_retry`.
  They always dialed a direct connection, which overrode any host-owned route.
  Open a client on the route you want and call `get_latest_block`,
  `get_tree_state`, or `anchor_tree_state_with_retry_on`.

## v3.1.0-rc.8

### Added
- Added a bundle- and round-independent PIR proof cache. `precompute_pir_proofs`
  fetches and persists IMT non-membership proofs for notes that survive the
  caller-supplied `BundlePolicy` (the same plan round setup uses: sub-ballot
  drop and privacy trim) against whatever snapshot the connected PIR server
  currently serves, keyed by `(wallet_id, network, root, nullifier)`, so
  wallets can warm proofs in the background from the selected snapshot set
  before any round is initialized, bundles exist, or a hotkey is generated.
  Padded-slot nullifiers are fetched later on the per-bundle path.
  `validate_cached_pir_proofs` classifies
  cached proofs against an expected round root offline (`Valid` / `StaleRoot`
  / `Missing` / `Invalid`). Snapshots coexist in the cache; leftover roots
  are unused, not harmful. New types: `PirCachePrecomputeResult`,
  `PirCacheValidationReport`, `PirProofCacheEntry`, `PirProofCacheStatus`.

### Changed
- The delegation prove path and `precompute_delegation_pir` now read and write
  the shared `pir_proof_cache` table instead of the bundle-scoped `imt_proofs`
  table, so background-warmed real-note proofs are never refetched at proving
  time; only the per-bundle padded-slot nullifiers can still require a fetch.
  A cached row that fails to decode or verify is treated as a miss and
  overwritten by the refetched proof, so a corrupt row self-heals instead of
  wedging the precompute. Schema version 15 migrates existing `imt_proofs`
  rows into the new cache (keyed by the owning round's network) and drops the
  old table.
- `precompute_pir_proofs` now prunes PIR proof cache rows created more than
  four weeks ago before warming the requested notes. Prove-time cache access
  remains non-pruning so an already cached proof can still complete a bundle.

## v3.1.0-rc.7

### Added
- Added `prepare_commit`, `prepare_commit_batch`, `persist_prepared_commit`,
  and `persist_prepared_commit_batch` so wallets can perform expensive ZKP #2
  proving outside SQLite transactions, then atomically persist the prepared
  result only if its vote-authority, ballot-intent, and current-vote state are
  still unchanged. `prepare_commit_batch` takes a `VoteCommitBatch` for the
  round, drafts, witness, and stage reporter.
- Added `warm_zkp2_proving_cache` for callers that want to initialize the vote
  proving parameters independently of the other proving caches.

## v3.1.0-rc.6

### Changed
- Updated the selectable cryptography facade to `voting-crypto-deps 0.1.2`,
  voting circuits to `0.10.3`, the indexed Merkle tree to `imt-tree 0.4.0`,
  and the PIR stack to `pir-types 0.5.0` and `pir-client 0.6.0`.
- Released `vote-commitment-tree 0.5.2` and
  `vote-commitment-tree-client 0.7.2` with backend-neutral field, group, and
  randomness trait imports for the updated upstream and Zakura dependency
  families.
- Updated the Zakura wallet stack to `zakura-wallet-lib 0.1.0-rc2`,
  `zakura-pczt 0.1.0-rc1`, `zakura-client-backend 0.1.0-rc2`,
  `zakura-client-sqlite 0.1.0-rc2`, and the `zakura-orchard`, `zakura-keys`, and
  `zakura-primitives` `1.0.0-rc.3` crypto family. These releases move the Zakura
  backend to `ff 0.14`, `group 0.14`, and `rand_core 0.10`.
- Routed the remaining test-only randomness imports through the selected backend
  facade (`voting_crypto_deps::rand`) instead of a direct `rand 0.8` dependency, so
  the same tests compile under both the upstream and Zakura families.

## v3.1.0-rc.5

### Added
- `VotingDb::effective_bundle_policy` is now public. A wallet that plans or
  reports outside the `*_for_round` helpers -- because its seed policy is not
  `BundlePolicy::default()` -- previously had no way to resolve a round's
  authoritative policy, and reconstructing the rule from the bundle count alone
  is wrong: a round planned by this binary stores a *trimming* policy, so
  treating "has bundle rows" as "no trim" under-reports withheld value for
  every round that was actually trimmed.
- `minimum_voting_eligibility_and_plan_for_notes` is now public, returning the
  eligibility status together with the `ChunkResult` it came from.
  `minimum_voting_eligibility_for_notes` computes the plan and discards it, so a
  wallet surfacing `PrivacyTrim` next to the eligible weight had to plan a
  second time and repeat the canonical duplicate-nullifier collapse to do it --
  two ways for the two numbers to start describing different note sets.

## v3.1.0-rc.4

### Changed
- **Breaking:** `BundleLayout` reports privacy-trim totals as flat fields
  (`privacy_trim_dropped_bundles`, `privacy_trim_dropped_notes`,
  `privacy_trim_dropped_value_zatoshi`) instead of a nested `PrivacyTrim`.
  Struct literals and JSON consumers must use the new names; absent fields still
  default to zero.
- **Breaking:** removed `privacy_trim` from `SignedDelegationBundle` and
  `SignedDelegationPayloadView`. Trim reporting stays on `BundleLayout` and
  `VotingNoteSelectionResultView` (`ChunkResult` is unchanged).

## v3.1.0-rc.3

### Added
- Accept `static_config_version: 2` static voting configs, which replace v1's
  single `dynamic_config_url` with an ordered `dynamic_config_urls` mirror list.
  `ResolvedStaticVotingConfig` gains `dynamic_config_urls` and
  `static_config_version`; `dynamic_config_url` is retained as the first mirror
  so v1 callers and every existing v1 hash pin keep working unchanged.
- Added `resolve_dynamic_voting_config_from_attempts`, which takes the wallet's
  ordered per-mirror fetch outcomes (`DynamicConfigAttempt`) and returns the
  first that resolves plus the mirrors it passed over
  (`DynamicConfigMirrorFailure`). A mirror is skipped when its fetch failed, its
  bytes did not decode, or its versions are unsupported; one that resolves but
  authenticates no rounds is deprioritized rather than skipped, so a round-less
  resolution is still returned when no mirror carries a verifiable round set.
  When no mirror resolves at all, the new `VotingConfigError::AllMirrorsFailed`
  enumerates every mirror and its reason; a one-mirror list, which is every v1
  static config, reports its own error verbatim instead — including the
  transport cause on a fetch failure, rather than a bare
  "dynamic config fetch failed".
  Falling back widens availability, not trust: every candidate is still
  authenticated against the static trusted keys, and the static hash pin is
  unchanged. Resolving from a non-first mirror emits the new
  `ConfigConditionKind::DynamicMirrorFallbackUsed` condition.
- Added `resolve_dynamic_voting_config_over_mirrors` and
  `DYNAMIC_MIRROR_FETCH_TIMEOUT` (30s): a reference lazy walk that bounds each
  mirror fetch so a blackholed primary cannot leave a healthy later mirror
  unused. The wallet-example and `config_fetcher` transports use it; wallets
  with their own networking should apply an equivalent per-attempt deadline.

### Changed
- `ConfigConditionKind::StaticHashPinVerified` now reports the real outcome. It
  previously reported `status: true` even when the static config source carried
  no `?checksum=sha256:` pin and no verification had run.

## v3.1.0-rc.2

### Added
- Privacy trim in bundle planning: trailing low-value bundles are dropped toward
  `BundlePolicy::max_privacy_bundles` (default 2) to shrink the observable
  delegation-submission count. The count is a target; the discarded value is
  bounded by two hard ceilings, `privacy_drop_bps` (default 1% of selected note
  value) and `max_privacy_drop_zatoshi` (default 1,000 ZEC).
- `PrivacyTrim` on `ChunkResult`, `BundleLayout`, `VotingNoteSelectionResultView`,
  and `SignedDelegationPayloadView`, reporting the raw note value withheld — not
  its bundle-quantized voting weight. Surface it rather than discarding voting
  power silently.
- Round-aware planning helpers that resolve a round's stored policy, so callers
  no longer supply policy internals: `voting_power_for_round`,
  `note_bundles_for_round`, `bundle_notes_for_index_for_round`, and
  `VotingNoteSelectionResultView::from_selected_for_round`.
- In-place upgrades for launched voting databases. Schema changes at or above the
  launch version apply ordered `ALTER` statements and keep persisted round state;
  only pre-launch databases are reset. A reset would have destroyed the randomly
  sampled `van_comm_rand` of any wallet upgrading between submitting a delegation
  and casting its vote, costing that round's weight unrecoverably.

### Changed
- **Breaking:** `BundlePolicy::default()` now trims. Opt out with
  `.with_max_privacy_bundles(None)` to keep the previous planning behavior.
- **Breaking:** added `privacy_trim` to `ChunkResult`, `BundleLayout`,
  `SignedDelegationBundle`, `SignedDelegationPayloadView`, and
  `VotingNoteSelectionResultView`. Struct literals must supply it; use
  `PrivacyTrim::default()` when no trim occurred. Serde-backed views still accept
  older payloads with the field absent.
- **Breaking:** `BundlePolicy::with_privacy_drop_bps` returns
  `Result<Self, VotingError>` and rejects budgets above `MAX_PRIVACY_DROP_BPS`.
- The effective `BundlePolicy` is persisted per round and becomes authoritative
  once stored, so an SDK upgrade that changes the defaults cannot invalidate
  bundle rows that were already signed or submitted. Rounds carried across the
  in-place upgrade have no stored policy, so the trim is disabled for any round
  that already holds bundle rows; they keep re-deriving the plan they were signed
  against.

### Removed
- **Breaking:** `VotingNoteSelectionResultView::from_selected` — use
  `from_selected_for_round`, which honors a resumed round's persisted policy.
- **Breaking:** `bundle_notes_for_index` — use `bundle_notes_for_index_for_round`,
  or `bundle_notes_for_index_with_policy` to pass a policy explicitly.

## v3.1.0-rc.1

### Changed
- Updated `zakura-client-backend`, `zakura-client-sqlite`, and
  `zakura-wallet-lib` to their coordinated `0.1.0-rc1` releases.

## v3.1.0-rc.0

### Added
- Added `share::pending_rounds` so wallets can restore unconfirmed helper-share
  tracking with the caller context persisted for each round.
- Extended `zcash_voting` with mutually exclusive `upstream` (default) and
  `zakura` features so the wallet layer can select crates.io librustzcash or the
  Zakura wallet-libraries forks via `zakura-wallet-lib`, in lockstep with the
  vote commitment tree crypto backend.

### Changed
- Replaced temporary Git dependency patches with the published backend-selector
  releases for IMT, PIR, voting circuits, and voting crypto dependencies.
- Released `vote-commitment-tree 0.5.1` and
  `vote-commitment-tree-client 0.7.1`, allowing both crates to select either
  the default upstream voting crypto backend or the mutually exclusive Zakura
  backend.
- Cap each randomized initial helper-share delay at 100 hours while preserving
  the round's last-moment safety window and retry timing from the sampled
  `submit_at`.

## v3.0.0

### Changed
- Released the exact `v3.0.0-rc.4` implementation as `v3.0.0` without
  implementation changes. Its supporting production snapshots were released
  as `pir-types 0.3.0`, `pir-client 0.4.0`, `voting-circuits 0.10.0`,
  `vote-commitment-tree 0.5.0`, and `vote-commitment-tree-client 0.7.0`.

## v3.0.0-rc.4

### Added
- Added a non-default `test-fixtures` feature exposing an atomic vote recovery
  fixture for downstream integration tests that should not build ZKP2.

### Changed
- Aligned the release line on stable `pir-types 0.3.0`, `pir-client 0.4.0`,
  `voting-circuits 0.10.0`, `vote-commitment-tree 0.5.0`, and
  `vote-commitment-tree-client 0.7.0` releases.
- Complete 16-share batch planning now spreads initial targets so that, when
  multiple helpers are configured, no helper is selected for every share.
  Fallback and recovery remain liveness first and may use any available helper.

## v3.0.0-rc.3

### Changed
- **Breaking:** `SharePayload` and `VoteShareWire` now carry the authoritative
  lowercase-hex `vote_round_id` from the vote commitment or recovery bundle.
  Wallets can submit the crate-produced helper-share JSON directly instead of
  injecting round context at the transport boundary.

## v3.0.0-rc.2

### Changed
- **Breaking:** extended the still-prerelease `auth_version: 2` round-auth
  payload to append `pir_layout.poly_len` as a `u32` in little-endian order.
  This binds the YPIR polynomial degree into each round attestation. Any v2
  signatures produced for `v3.0.0-rc.1` used the shorter preimage and must be
  regenerated before wallets adopt this release.
- Updated the PIR client stack to `pir-types 0.3.0-rc.6`,
  `pir-client 0.4.0-rc.7`, and `valar-ypir 0.2.0`. Dynamic voting config
  `pir_layout` now includes `poly_len` (`2048` or `4096`), and PIR connection
  passes the full layout into the server handshake. It fails closed before any
  private query when `/root.pir_layout` or `GET /params/tier1` disagrees.

### Fixed
- Restored negotiated PIR layout support after `v3.0.0-rc.1` inadvertently
  restricted wallets to the current production default. Dynamic config and
  direct PIR connection again accept layouts supported by the shared client
  capability predicate while requiring an exact config/server match before any
  private query. Snapshot tooling and fleet deployment remain responsible for
  advertising only layouts they can materialize, so compatible service layout
  changes do not require a wallet release.

## v3.0.0-rc.1

### Added
- Added secret-free, round-bound voting hotkey targets and a canonical
  delegation capability handoff. A funds controller, such as a custody
  provider, can prepare delegation for a voter's public target, durably store
  the package before broadcasting, verify delivery by its digest, and let the
  voter use the existing confirmation, tree-sync, and ZKP2 voting path without
  sharing account viewing material or the voting hotkey secret. Imported
  capability rounds keep their complete bundle batch atomic and wait for every
  delegation bundle to confirm before creating fresh vote commitments, keeping
  pre-vote package replacement recoverable.

### Changed
- **Breaking:** dynamic voting config round authentication now requires
  `auth_version: 2`. The trusted-key Ed25519 signature covers the canonical
  fixed-width encoding of `RoundAuthPayloadV2`, whose fields encode as
  `"zcash-shielded-vote:round-auth:v2" || round_id (32 raw bytes decoded from
  the rounds-map key) || ea_pk (32 bytes) || pir_depth (u32 LE) ||
  tier0_layers (u32 LE) || tier1_layers (u32 LE)` instead of the bare `ea_pk`.
  This binds each attestation to its round and to the advertised PIR layout, so
  a compromised config host can neither replay a signed `ea_pk` under a
  different round id nor swap the `pir_layout` under attested rounds (a layout
  change requires re-signing every active round).
  `auth_version: 1` entries are no longer authenticated and are reported in
  `skipped_round_ids`; round entries must be re-signed with vote-sdk tooling
  that emits v2 before wallets adopt this release.
- **Breaking:** config resolution and direct PIR connection now accept only the
  deployed 19/12/7 layout currently produced by the production snapshot
  tooling, exposed as `PirLayout::DEPLOYED`. Negotiated geometry is still
  validated first with the shared `pir-types` supported-layout predicate so
  malformed layouts retain detailed validation errors.
- Aligned the prerelease family on `voting-circuits 0.10.0-rc.1`,
  `vote-commitment-tree 0.5.0-rc.1`, and
  `vote-commitment-tree-client 0.7.0-rc.1`.

### Fixed
- Vote commitment tree sync now exposes `SyncLimits` and
  `TreeClient::sync_with_limits`, with defaults of 4,096 pages and five minutes
  per complete sync. The built-in wallet and `vote-tree-cli` transports bound
  each HTTP response to 8 MiB and each request to 60 seconds. Per-round client
  locks prevent a stalled node from blocking tree operations for unrelated
  rounds in the same wallet.

## v2.0.0

### Changed
- Released the exact `v2.0.0-rc.5` implementation as `v2.0.0` without
  implementation changes. Its supporting production snapshots were released
  as `voting-circuits 0.9.0`, `vote-commitment-tree 0.4.0`, and
  `vote-commitment-tree-client 0.6.0`.

## v2.0.0-rc.5

### Fixed
- Keystone signing requests now mark deliberate zero-value hotkey outputs with
  their user-facing address so signer devices display the bundle memo.

## v2.0.0-rc.4

### Changed
- Published `vote-commitment-tree 0.4.0-rc.2` and
  `vote-commitment-tree-client 0.6.0-rc.2` with the workspace's
  `imt-tree 0.2.1` dependency.
- `pir::connect_pir` / `pir::connect_pir_blocking` now take an explicit
  `PirLayout` and fail closed on config/server layout mismatch before any
  private query (`VotingError::InvalidInput`). Clients accept any valid
  two-tier layout matching `/root` rather than a compiled-layout gate.
  `COMPILED_PIR_LAYOUT` is no longer re-exported from `zcash_voting` /
  `prelude`; use resolved config `pir_layout` (tests may still import from
  `pir-types`).
- Dynamic voting config now requires top-level `pir_layout` (`pir_depth`,
  `tier0_layers`, `tier1_layers`). `ResolvedVotingConfig` and its wire exports
  expose it; layout changes are same-chain service updates.
- Delegation submissions now carry compact, versioned Ironwood transaction
  effects so verifiers derive the signing digest directly instead of receiving
  it as a separate field. The payload excludes PCZT signer metadata, and
  synthetic outputs omit account-scoped outgoing viewing metadata. Synthetic
  signing PCZTs also leave their unused V6 anchor and spend witness unset.
- Changed vote-share wire JSON to include only the encrypted share assigned to
  the receiving helper. The `all_enc_shares` field is no longer serialized.

## v2.0.0-rc.3

### Changed
- `NoteInfo::from_orchard_note` now rejects non-Ironwood/V3 notes with
  `VotingError::InvalidInput`. Voting is Ironwood-only, but `NoteInfo` does not
  carry the note version, so an Orchard/V2 note previously passed ingestion and
  bundling and failed only during proof construction — after the governance PCZT
  had been built and signed.
- Updated the published librustzcash dependency requirements to `pczt 0.9.2`,
  `zcash_client_backend 0.24.0-rc.7`, `zcash_client_sqlite 0.22.0-rc.7`,
  `zcash_keys 0.16.1`, and `zcash_protocol 0.10.4`. Orchard remains on the
  compatible `0.15` line so downstream workspaces select their own patch release.
- Updated the real delegation proof fixture to use Ironwood/V3 notes and run the
  ignored Halo2 proof tests under the release profile in CI.

## v2.0.0-rc.2

### Changed
- Published the retained-checkpoint vote tree on `shardtree 0.7` as
  `vote-commitment-tree 0.4.0-rc.1`, with the aligned
  `vote-commitment-tree-client 0.6.0-rc.1` release.
- Updated the librustzcash dependency family to published
  `zcash_client_backend 0.24.0-rc.4`, `zcash_client_sqlite 0.22.0-rc.4`, and
  `pczt 0.9.1`.

## v2.0.0-rc.1

### Changed
- Updated the librustzcash dependency family to published `zcash_primitives 0.30.0`,
  `zcash_keys 0.16.0`, `zcash_client_backend 0.24.0-rc.2`,
  `zcash_client_sqlite 0.22.0-rc.2`, and `pczt 0.8.0`.
- Aligned the Ironwood crate line on `voting-circuits 0.9.0-rc.3`,
  `pir-client 0.4.0-rc.2`, and `pir-types 0.3.0-rc.2`. Wallet integrations
  resolve one shielded/PCZT stack, vote tree storage APIs use `shardtree 0.7`,
  and the wallet crates require Rust 1.88 or newer.
- Updated snapshot selection and governance PCZT construction to use only
  Ironwood/V3 notes. Pre-NU6.3 Orchard/V2 voting is no longer supported on this
  branch, and Ironwood voting no longer requires a custom Rust compile flag.
- Changed public round initialization and delegation APIs to require an explicit
  wallet network, which is persisted with the round state.
- Moved governance PCZT construction behind `VotingDb::build_governance_pczt`,
  which validates branch IDs against the stored round snapshot before writing
  PCZT setup state.

## v1.0.0

### Added
- Added an optional `BundlePolicy` threshold that starts a new bundle when
  adding a note would push the current bundle over the threshold.
- Added shared last-moment round timing helpers in `share_policy` so wallet
  integrations can derive the same helper-share buffer, deadline, and
  `single_share` decision from ceremony start and vote end times.
- Added the public `VOTING_HOTKEY_STORED_SECRET_LEN` constant and updated v2
  hotkey guidance so software and hardware wallets both use app-owned random
  hotkeys instead of deriving software hotkeys from wallet seed material.
- Added crate-owned FRB DTO views in `zcash_voting::wire` for wallet API
  surfaces that previously used local mirrors in `vizor-wallet`:
  `VotingNoteRefView`, `VotingNoteSelectionResultView`, `BundleSetupResultView`,
  `DelegationPirPrecomputeResultView`, `SignedDelegationPayloadView`,
  `KeystoneDelegationRequestView`, `KeystoneSignatureRecordView`, `DraftVoteView`,
  `VanWitnessView`, `SignedVoteCommitmentView`, `SignedVoteCommitmentsView`,
  and `VoteRecordView`.
- Added stable resume-plan wire DTOs in `zcash_voting::wire`
  (`NextStepView`, `RoundPlanView`) so wallet adapters can consume crate-owned
  `session::resume_plan` outputs directly over FRB without maintaining local
  `ApiRoundPlan`/`ApiNextStep` mirrors.
- Added stable recovery/scheduling wire DTOs under `zcash_voting::wire` so wallet
  adapters can share one serde-backed JSON shape for recovery snapshots and
  share submission planning (`ShareSubmissionPlanView`,
  `DelegationRecoveryView`, `VoteRecoveryView`,
  `CommitmentBundleRecoveryView`, `ShareWorkflowRecoveryView`,
  `ShareDelegationRecordView`, and `RoundRecoveryStateView`).
- Added wallet-sidecar and round-context convenience APIs so SDK adapters can
  reuse crate-owned voting DB/session policy instead of local wrappers:
  `VotingDb::wallet_sidecar_path`, `VotingDb::open_wallet_sidecar`,
  `VotingDb::ensure_round_state`, and `delegate::ensure_round_context`
  (`DelegationRoundContext`).
- Extended `session::RoundPlan` with crate-owned recovery/display projection
  fields (`blocking_recovery`, `blocking_share_work`,
  `completed_vote_artifact`, `completed_for_display`, `needs_draft_setup`,
  `primary_action`, `delegation_statuses`, `completed_vote_display`, grouped
  `recovered_delegation_work`, and grouped `recovered_vote_work`) so wallet
  integrations can stop rebuilding foreground-blocking, "voted" display,
  delegation phase, hotkey reuse, vote recovery completeness, delegation
  polling, vote polling, recovered vote submission, and blocking share retry
  decisions from raw recovery snapshots. The same projection is exposed through
  `wire::RoundPlanView` for FFI consumers.
- Added canonical wire JSON types in `zcash_voting::wire`
  (`DelegationSubmissionWire`, `VoteCommitmentWire`, `VoteShareWire`,
  `WireEncryptedShareJson`) so wallets can reuse one source of truth for
  protocol field names, serde renames, and base64/JSON-safe shaping instead of
  reimplementing submission serializers. The wire API now includes
  `VoteShareWire::with_late_bound` for safely applying runtime
  `tree_position`/`submit_at` values while preserving the crate-owned JSON
  integer bounds checks.
- Added a stable `recovery` reporting API so wallets can fetch one typed round
  snapshot from `zcash_voting` instead of reassembling recovery state with
  low-level SQL. New exports include `recovery::round_snapshot`,
  `recovery::recoverable_commitment_bundle`, and `recovery::clear`, plus
  prelude re-exports and a wallet example (`wallet-example::example_recovery`)
  that pairs snapshots with `session::resume_plan`.
- Added `vote::SignedVoteCommitments`, `vote::commit_batch`, and
  `vote::recover_signed_commitments` so wallet SDKs can commit and recover
  per-bundle cast-vote batches through one crate-owned entry point instead of
  reimplementing per-draft loops and recovery wrapping.
- Added atomic idempotent recovery writers on `VotingDb`:
  `mark_delegation_submitted` and `mark_vote_submitted`, including conflict
  checks for tx hashes.
- Added `vote::SignedVoteCommitment` plus
  `CommittedVote::signed_commitment` as the canonical wallet-facing aggregate
  for cast-vote outputs. The API now exposes submission fields, helper-share
  payloads, and persisted recovery JSON through one typed surface with
  fixed-size cryptographic fields (`[u8; 32]` / `[u8; 64]`) to keep byte-length
  guarantees inside the shared crate while still supporting boundary adapters.
- Added `VanWitness::from_wire` to validate and convert wire-friendly witness
  siblings into the typed `[[u8; 32]; 24]` witness form used by vote
  commitment APIs.
- Added `vote::CommittedVote`, a stateful cast-vote handle that mirrors the
  `PreparedDelegationBundle` method flow. Wallet SDKs can now commit/recover a
  vote once, then drive submission and helper-share lifecycle steps through
  methods (`submission`, `share_payloads`, `record_share`, `confirm_share`,
  `add_sent_servers`, `record_submission`, `record_vc_position`) without
  re-threading `(round_id, bundle_index, proposal_id)` across free functions.
- Added `DelegationSigningRequest`, `delegation_signing_request`, and generic
  external delegation signature constructors so wallet SDKs can keep wallet seed
  material outside `zcash_voting`, sign the PCZT sighash locally with the account
  SpendAuth key, and pass only the resulting signature back to the crate.
- Added shared draft vote bounds validation for SDK integrations. The crate now
  exposes proposal and option count bounds plus `vote::validate_draft_vote(s)`,
  and `vote::commit` rejects invalid drafts before proof construction.
  `VotingDb::set_ballot_intent_for_draft_vote` records choice intent through
  the same validated draft surface, and direct ballot-intent writes now require
  the proposal's option count so choices are validated before persistence.
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
- Added `vote::submission` / `vote::recover_commit` guidance for
  `NextStep::SubmitVote` handling. Wallets can reconstruct cast-vote
  submission fields from persisted recovery state without rebuilding from a
  draft, then call `recover_commit` again after confirmation to recover
  helper-share payloads with the confirmed VC position.
- Added `confirmation::*` APIs for wallet SDKs to parse confirmed
  `delegate_vote` and `cast_vote` tx events, then atomically record delegation
  tx hashes, VAN positions, cast-vote tx hashes, and vote commitment tree
  positions without writing workflow SQL locally.
- Added shared delegation request/report types, account-key loading, Keystone
  PCZT redaction, display memo formatting, skipped-suffix bundle validation,
  and bundle weight helpers so wallet SDKs can keep only their runtime-specific
  async/lightwalletd shims.
- Added shared `lwd` helpers for mainnet lightwalletd channel setup, bounded
  unary RPCs, chain-tip lookup, consensus branch resolution, and snapshot
  `TreeState` fetching with retry so wallet SDKs no longer need local copies of
  these queries.
- Added shared wallet note-selection helpers and delegation input gathering
  (`select_snapshot_notes`, `select_snapshot_note_infos`, and
  `gather_delegation_wallet_inputs`) so wallet SDKs can reuse the snapshot
  eligibility, shielded note-info extraction, and selected-note summary logic.
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
- Added voting hotkey helpers for app-owned random hotkeys, stored hotkey secret
  reconstruction, raw Orchard delegation-address derivation, and typed
  `DelegationKeys` / `VoteSigner` helpers. New wallet SDKs should generate a
  random hotkey once, store `VotingHotkey::stored_secret()`, and reconstruct a
  typed `VotingHotkey` with `VotingHotkey::from_stored_secret` when needed.
  `generate_random_voting_hotkey` replaces the older raw hotkey generation
  helpers exposed through `hotkey::generate_hotkey` and
  `VotingDb::generate_hotkey`.
- Added `delegate::LightwalletdBranchIdProvider` and
  `delegate::branch_id_for_height` so wallet SDKs can resolve delegation
  consensus branches from a voting snapshot height plus `Network` without
  duplicating consensus activation logic.
- Added `vote::VoteCommitStage` plus `VoteCommitStageReporter` and
  `VoteCommitStageBridge` so wallet SDKs can consume library-owned cast-vote
  lifecycle and proof-progress stages without defining local event enums.
- Added `VotingDb::prepare_delegation_pir` so wallet SDKs can share the
  delegation bundle validation, governance PCZT construction, and PIR precompute
  sequence while still supplying wallet-specific notes, account metadata, typed
  voting hotkey, consensus branch, and PIR transport at their own boundaries.
  Callers that need a non-default bundle policy can use
  `VotingDb::prepare_delegation_pir_with_policy`.
- Added `zcash_voting::witness::generate_note_witnesses` for shielded note
  witness generation from a stored voting round snapshot. The API selects the
  shielded voting protocol from the snapshot height, validates the cached
  lightwalletd `TreeState` height and selected tree root against the persisted
  round parameters, asks the wallet DB for historical Merkle paths, then returns
  `WitnessData` for each bundled note.
- Added `zcash_voting::witness::store_tree_state_and_generate_note_witnesses`
  so wallet SDKs can share the snapshot tree-state persistence, witness
  generation, and bundle witness caching flow while keeping wallet DB opening at
  each SDK boundary.
- Added `VotingDb::has_witnesses` so wallet SDKs can detect already-cached
  bundle witnesses and skip repeat witness generation during precompute resume.
- Added `delegate::prepare_delegation_bundle` with
  `PrepareDelegationBundleParams` and `PreparedDelegationBundle` so wallet SDKs
  can resolve lightwalletd inputs before opening wallet DB handles, then reuse
  plain bundle state across witness precompute, PIR warmup, signing, and
  Keystone flows.
- Added `PreparedDelegationBundle` lifecycle methods and `PreparedSigner` so
  precompute, PCZT setup, proof generation, Keystone signing requests, and
  submission assembly all consume the same prepared bundle state instead of
  re-threading loose round IDs, bundle indexes, note lists, and keys. The
  prepared lifecycle now also owns software-wallet delegation signing for
  callers that choose to pass a seed to the crate, external signature byte
  validation, witness-cache checks, and signed-payload metadata.
- Added `vote::validate_draft_votes` so wallet SDKs can validate canonical
  `DraftVote` inputs through the shared voting API before DB or proof work.
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
- Added `precompute::reset_voting_session_state` and
  `VotingDb::clear_unsigned_delegation_setup_fields` so wallet integrations can
  recover from interrupted Keystone delegation setup after process restart.
  Round-scoped reset drops process-local vote-tree cache and clears unsigned
  delegation setup columns (`pczt_sighash`, padded-note secrets, and related
  transient fields) while preserving bundles with Keystone signatures or a
  stored `delegation_tx_hash`.
- Added a `zcash_voting::config` voting-service config resolution API
  (`resolve_static_voting_config`, `resolve_dynamic_voting_config`, and
  `decide_config_switch`) so wallets can authenticate static/dynamic config
  bytes with their own transport and classify the resulting config switch.
  `resolve_static_voting_config(source, static_bytes)` authenticates the static
  trust anchor and exposes the `dynamic_config_url` to fetch next;
  `resolve_dynamic_voting_config(resolved_static, dynamic_bytes, options)` then
  authenticates the dynamic config against it. A `wallet-example::example_config`
  module pairs these with a direct-HTTPS `DirectHttpsFetcher` and persists the
  resolved summary used for later switch decisions.
- Added `examples/end_to_end_vote.rs` and README notes for moving from the
  delegation-oriented V2 API to the new vote/share API.

### Changed
- Keystone delegation memo display (`delegate::display_memo`) now puts voting
  power on its own `Amount:` line and truncates only the round name with
  UTF-8-safe byte boundaries when the memo approaches the 512-byte signer limit,
  so hardware-wallet displays no longer clip the trailing ZEC amount. Governance
  PCZT memo bytes now reuse the same formatter.
- `delegate::prepare_delegation_bundle` now takes a typed `VotingHotkey` and
  owns wallet scanned-height lookup. Callers no longer pass hotkey seed bytes
  through delegation preparation.
- Consolidated delegation-bundle preparation into one public
  `delegate::prepare_delegation_bundle` path. `PrepareDelegationBundleParams`
  now carries `DelegationLwdInputs` (`lwd`) and `session_json` directly, while
  `wallet_db` is an explicit function argument instead of being embedded in the
  params struct.
- The default feature set now enables both `pir` and `tree-sync`, so the
  built-in network client surface is available without extra feature flags.
- `zcash_voting::transport::HyperTransport` is now exported unconditionally.
  Callers no longer need to enable `pir`, `tree-sync`, `client-pir`, or
  `client-tree-sync` just to access the transport re-export.
- Split wire DTO definitions from codec/conversion logic: `zcash_voting::wire`
  now owns stable protocol structs only, while serde/base64 conversion helpers
  and JSON-shaping tests moved into crate-private `wire_codec`.
- Moved `VotingRoundParams` ownership to `zcash_voting::wire` as the canonical
  vote-chain payload type, while re-exporting it from `types` for downstream
  compatibility.
- Consolidated wallet-facing recovery orchestration into crate-owned APIs:
  added `phases::WorkflowPhase` with stable resume strings, exposed
  `workflow_phase()` accessors on recovery records, and updated the wallet
  recovery example to load snapshot + `resume_plan` together and recover
  committed-vote payloads directly from planner steps.
- Consolidated recovery snapshot assembly and pending commitment-bundle
  semantics into `zcash_voting`, and reduced the wallet-side adapter boundary
  to FFI shape/phase-string mapping. Added focused unit coverage for pending
  commitment rows, sidecar reopen behavior, and recovery clearing invariants.
- The wallet vote example now includes `commit_vote_bundle_batch`, showing the
  canonical batch cast-vote flow with `vote::commit_batch` and crate-owned
  cancellation/progress adapters.
- Removed crate-side wallet seed APIs from voting hotkey derivation and
  prepared delegation signing. Wallet SDKs now generate app-owned random hotkeys
  and delegation signatures locally, then pass typed `VotingHotkey` values or
  SpendAuth signatures into the crate.
- Removed unused legacy APIs left behind by the wallet integration refactor:
  direct share decomposition/encryption modules, the public share-tracking
  nullifier module, legacy `VotingDb` hotkey helpers, confirmed-state writer
  shims, legacy `Network` numeric converters, and the mainnet-only consensus
  branch ID helper.
- Delegation PIR warmup no longer constructs or caches a governance PCZT.
  `PreparedDelegationBundle::precompute` now warms witnesses, padded-note
  secrets, and PIR rows only; `delegate::setup` builds the PCZT later from the
  persisted padded secrets and refuses to overwrite existing padded secrets or
  `pczt_sighash`. The old loose `PrecomputeDelegationInputs` entry points were
  removed in favor of the prepared-bundle lifecycle.
- Removed the process-local prepared-PCZT cache and its prelude exports now that
  precompute no longer builds PCZT setup material.
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
- `VotingHotkey` now represents the actual stored hotkey secret plus raw Orchard
  address. The old placeholder Pallas public key and `sv1...` address fields
  were removed.
- `VoteSigner` now accepts only a typed `VotingHotkey`, and
  `vote_commitment::sign_cast_vote_for_account` was removed in favor of the
  canonical voting hotkey account index.
- Raw-byte `DelegationKeys` construction is no longer public. Wallet callers use
  `DelegationKeys::with_voting_hotkey`, and the crate derives network-specific
  metadata from the `VotingHotkey`.
- Low-level ZKP2 and cast-vote signing helpers that take raw hotkey seed plus
  `network_id` are now crate-internal. Wallet callers should use `vote::commit`
  with `VoteSigner`.
- The wallet delegation example now separates reusable bundle preparation from
  PIR precompute, software signing, and Keystone request/submission helpers so resume
  flows can share cached bundle state without repeating lightwalletd and wallet
  note-selection work.
- `delegate::redact_for_signer` is no longer exported as a generic wallet-facing
  helper. Delegation Keystone requests still redact their PCZT internally;
  generic wallet send PCZT redaction belongs in the wallet SDK boundary.

# 0.11.0

## Changed
- Bumped `zcash_voting` to `0.11.0`, `vote-commitment-tree` to `0.3.2`,
  and `vote-commitment-tree-client` to `0.5.2`.
- Bumped the Orchard dependency line to `orchard 0.14`,
  `halo2_gadgets =0.5.0`, `pczt 0.7`, `zcash_keys 0.14`,
  `zcash_primitives 0.28`, and `zcash_protocol 0.9`.
- Bumped the circuit and nullifier dependencies to published
  `voting-circuits 0.8.0`, `imt-tree 0.2.0`, `pir-types 0.2.0`, and
  `pir-client 0.3.0`.

# 0.10.2

## Security
- Bumped `voting-circuits` to `0.7.0`, which rejects Halo2 proofs that verify
  but leave trailing unread transcript bytes.

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
