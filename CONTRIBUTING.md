# Contributing

## Build and test

Use the `make` targets; they are the only supported entry points. `make check`
then `make test` is the normal cycle and is enough to consider most changes
verified. `make help` lists every target, and
[`AGENTS.md`](AGENTS.md) explains when the heavier suites
(`make test-lrz`, `make test-vct`, `make proofs`) are worth running.

Do not run bare `cargo build` / `cargo test` / `cargo check`: the `zakura` and
`lrz` features pull two mutually exclusive crypto stacks, and the `make` targets
pin a separate `CARGO_TARGET_DIR` per permutation to avoid full-graph rebuilds.

## Code standards

[`AGENTS.md`](AGENTS.md) is the single source of agent and contributor
instructions for this repository, including naming, module layout, and test
placement. `zcash_voting/src/share_policy/` and
`zcash_voting/src/share_tracking/` are the in-repo examples of the target shape.

Before changing helper-share planning, submission, transport, persistence,
polling, or recovery, read
[`docs/helper_submission_invariants.md`](docs/helper_submission_invariants.md).

## Releases and backports

`main` is the development line for the next release. Shipped major series are
maintained on `release/vMAJOR.x` branches, and semver-compatible fixes reach them
through reviewed automated backports rather than direct pushes. Read
[Release branches and backports](docs/release-branches.md) before deciding where
a change lands or applying a backport label.
