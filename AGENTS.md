# Repository agent instructions

## Helper-share submission

Before changing helper-share planning, submission, transport, persistence,
polling, or recovery, read and follow
[`docs/helper_submission_invariants.md`](docs/helper_submission_invariants.md).

The document is the review specification for the invariants currently enforced
by this repository. In particular:

- do not weaken or bypass an invariant silently;
- preserve durable state transitions, timeout and retry boundaries, helper
  placement rules, and ambiguous-POST safety;
- update the specification and its cited regression tests in the same change
  whenever behavior intentionally changes; and
- explicitly report any conflict between a requested change and the
  specification before implementing it.

These instructions apply to all files that can affect helper-share behavior,
including the `zcash_voting/src/share_policy/` and
`zcash_voting/src/share_tracking/` facade packages,
`zcash_voting/src/share.rs`, `zcash_voting/src/recovery.rs`,
`zcash_voting/src/helper/`, `zcash_voting/src/storage/queries/`, and
helper-share wire representations.

## Chain submission

Before changing vote-chain broadcast, transaction lookup, submission attempt
journaling, retry and failover, nullifier interpretation, confirmation
persistence, or the recovery cleanup that protects them, read and follow
[`docs/chain_submission_invariants.md`](docs/chain_submission_invariants.md).

That document is the review specification for the invariants currently enforced
by this repository, and the same rules apply as for helper shares: do not weaken
or bypass an invariant silently, preserve durable state transitions and
ambiguous-POST safety, update the specification and its cited regression tests in
the same change whenever behavior intentionally changes, and report any conflict
before implementing it.

These instructions apply to `zcash_voting/src/chain/`,
`zcash_voting/src/chain_submission.rs`, `zcash_voting/src/confirmation.rs`, the
`chain_submission_attempts` schema and its migrations, and the ballot-intent and
recovery-cleanup paths in `zcash_voting/src/session.rs`,
`zcash_voting/src/vote.rs`, and `zcash_voting/src/storage/queries/`.
