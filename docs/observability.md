# API observability

Observability is optional and additive. Existing methods retain their original
arguments and return types, including normal `Result` handling with `?`,
`.map_err`, and `collect`. Selected workflow boundaries provide a
`*_with_report(..., options: Option<ObservabilityOptions>)` counterpart.
`Some(ObservabilityOptions::default())` enables collection for that invocation;
`None` disables it. There are no observability builders or inherited settings. Existing callers need
no migration; only callers adopting diagnostics use reported methods.

Reported calls return `OperationReport<T>`, with the authoritative domain result
in `result` and an optional frozen snapshot in `observability`. For a fallible
workflow, `T` is `Result<Value, Error>` so diagnostics remain available on error.
Use `into_parts()` to retain diagnostics before propagating the error:

```rust,ignore
let report = RoundDriver::new(&executor)
    .run_with_report(
        &host,
        &control,
        &events,
        Some(ObservabilityOptions::default()),
    )
    .await;
let (round, diagnostics) = report.into_parts();
if let Some(diagnostics) = diagnostics {
    println!("{diagnostics}");
    save_diagnostics(diagnostics);
}
// `round` has the same domain type as driver.run(...).await.
```

Each reported invocation owns its collector. Repeated or concurrent calls on the
same client have independent reports. Internal child contexts propagate through
nested SDK calls and worker threads without producing nested reports or gaining
finalization ownership. The opaque, hidden context on `DelegationDriver`'s
defaulted extension hooks is only for SDK forwarding. Existing trait
implementations need no changes and receive an outer duration measurement.

## Coverage and exclusions

Reported entry points cover round and share-tracking drivers, delegation pipeline
setup/preparation/PIR/proof workflows, independent delegation preparation and
proving, snapshot precomputation, and direct chain/helper delivery workflows.
Utilities do not acquire a parallel reported API merely because they are timed
inside a workflow.

| Area | Nested measurements |
| --- | --- |
| Delegation preparation | Round initialization, wallet selection and keys, bundle preparation, witnesses |
| PIR | Precomputation, cached-proof validation, delegation PIR work |
| ZKP1 | Circuit construction, proving-key acquisition, proof generation |
| ZKP2 | Vote preparation and proof generation, including atomic-batch workers |
| Persistence and recovery | Workflow persistence and recovery boundaries |
| Chain | Advancement, POST attempts, status attempts, exact-tree recovery requests |
| Helper shares | Submission, status polling, recovery, HTTP attempts and retry waits |
| Round execution | Run and per-step outcomes, blocking work including vote-tree sync |

`has_persisted_proof`, signature extraction, witness/selection utilities, and
payload reconstruction retain plain return types. `gather_delegation_lwd_inputs`
and `recovery_bundle` are excluded. `sync_vote_tree` and `sync_vote_tree_with`
retain their original APIs; timing belongs to their production SDK caller.
Examples do not add tree-sync instrumentation.

## Reading reports

`started_at_unix_us` anchors the invocation to app/server logs. Durations and
relative `started_after_us` offsets use a monotonic clock. Records include stage,
parent ID, elapsed microseconds, outcome, and known bundle/proposal/share indexes.
Round-aware boundaries populate `round_id`. Network attempts carry a one-based
`attempt` within their operation, plus HTTP status and configured endpoint ordinal
when available. Attempts from different parent operations are separate sequences.
Reports exclude payloads, URLs, keys, proof bytes, signatures, and free-form errors.

Atomic batch casting uses these existing reports. Batch preparation, persistence,
recovery and chain advancement carry bundle attribution without a proposal or
share ID. The enclosing round step may name the proposal that triggered the
batch; each proof worker and helper operation names its own proposal and share
when known. Worker records share the invocation's collector and parent linkage.
Fresh-cast handle recovery is included alongside resumed recovery. A batch route
failure retains the chain lifecycle's dispatch uncertainty; reports do not
introduce singleton fallback or change recovery decisions. A rejected batch POST
can return pending recovery: its attempt is `rejected` while the enclosing
operation remains `pending`. An ambiguous response likewise retains its
`possibly_dispatched` attempt alongside the authoritative recovery result.

Outcomes reflect domain results: cancellation is not success, pending helper
shares are not confirmed, and hashless chain submissions remain
`possibly_dispatched` through enclosing reports. Always use the authoritative
domain result for retry and recovery decisions. Diagnostics are explanatory and
are never persisted as lifecycle state.

Snapshots are frozen on return. Work still running is `unfinished`, with elapsed
time clipped at that boundary. Detached workers cannot mutate returned reports.
Parent and child durations overlap; summing records does not give wall time.
`Display` supplies a default readable rendering. The crate root and prelude export
options, the report envelope, and all diagnostic DTOs including records/summaries.
Rust stage/error labels share interned `Arc<str>` allocations; wire DTO projections
use owned strings and serialize to the same JSON representation.

## Limits and schema stability

Defaults independently cap detailed records, summary groups, and simultaneously
active timers at 4096 each:

- `max_records` retains details in start order. `records_dropped` counts omitted
  detail records; retained children have retained ancestors.
- `max_summary_groups` caps distinct `(stage, attribution, outcome)` rows. Existing
  rows keep accumulating after the cap. `summary_updates_dropped` counts omitted
  measurements for groups that could not be admitted, not distinct omitted groups.
- `max_active_stages` caps concurrent timers. `active_stages_dropped` counts stage
  starts that could not be admitted; their work still executes normally. Subsequent
  admitted descendants attach to the nearest admitted ancestor.

`ObservabilityOptions::summaries_only()` sets `max_records` to zero while retaining
bounded, outcome-specific summaries. Zero summary/timer limits omit that collection
and increment their counters; they do not suppress the operation snapshot. Only
`None` disables collection entirely. Limits bound collector cardinality, not a
fixed byte budget or the application's concurrency. Static SDK labels are interned;
caller-provided stage/error strings cannot expand that vocabulary.

The record, summary, and operation output structs (including wire projections)
are `#[non_exhaustive]`: consumers should access fields or destructure with `..`,
and allow future output fields. Options remain constructible with `Default`.

DTO field names, duration units, outcome discriminators, and error categories are
part of the SDK API. Stage labels identify implementation phases: their names,
nesting, counts, and granularity may evolve. Consumers must accept unknown stages
and missing optional metadata. Keep SDK version alongside serialized reports and
build dashboards around domain outcomes and attribution rather than a fixed stage
sequence. No chain/helper request format or voting database schema changes are
required.

## Report coverage and maintenance checklist

Coverage is intentional: a new public utility does not automatically need a
reported counterpart. Client workflow entry points do. When adding or changing a
public workflow, review this checklist alongside its plain method:

- Decide whether clients need standalone diagnostics; record intentional exclusions
  in this guide. Check actual client call paths, including free-function alternatives
  to pipeline methods.
- Provide `*_with_report(..., Option<ObservabilityOptions>)` for an observed boundary.
  Keep the plain method's signature unchanged and route both through the same
  internal implementation. Do not duplicate validation, side effects, or retry logic.
- Keep outcome classification aligned between the outer report and its operation
  stage, including cancellation, reuse, and uncertain dispatch.
- Bind all known identities before fallible work. Round-independent cache warm-up
  intentionally has no round or bundle identity.
- Pass borrowed child contexts through internal calls. Only the reported entry
  point creates and finalizes an invocation; children never create nested reports.
- Test enabled and disabled calls against the plain result, including errors and
  actual downstream work. Retain diagnostics on failure, and verify known identity.
- Update this coverage description, API documentation, and changelog. Run
  `make check`, `make test`, and `make fmt` before merging.

In particular, client preparation paths include
`DelegationPipeline::eligibility_with_report`,
`DelegationPipeline::keystone_request_with_report`, and the free
`precompute::precompute_pir_proofs_with_report`. The latter observes cache warm-up
before a round exists, independently of `DelegationPipeline::precompute_pir_with_report`.

`ObservationScope` is a hidden public type solely for the defaulted
`DelegationDriver::prove_and_sign_blocking_observed` and
`DelegationDriver::resign_blocking_observed` extension hooks. Those public
signatures require a publicly reachable type. Clients neither construct nor
finalize it; normal reported operations accept options instead.
