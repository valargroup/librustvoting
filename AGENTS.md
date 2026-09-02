# Repository agent instructions

This file is the single source of agent instructions for this repository.
`CLAUDE.md` is a symlink to it, so Claude Code, Codex, and Cursor all read the
same text; edit `AGENTS.md` and never add a second instructions file.

Tool-specific wiring, all pointing at the same shared content:

- `.agents/skills/` holds the shared skill definitions.
- `.claude/skills/` and `.cursor/skills/` symlink into `.agents/skills/`.
- Codex has no skill loader, so anything it must follow is stated in full in
  this file, with a link to the source of truth.

## Build and test

Use the `make` targets. They are the only supported entry points:

**Default loop — use these:**

| Command | What it does |
| --- | --- |
| `make check` | Type-check the default Zakura stack. The fast inner loop. |
| `make test` | Run the default Zakura test suite. The default gate. |
| `make clippy` / `make fmt` | Lint and formatting checks. |

**Run only when the change calls for it, or when asked:**

| Command | When |
| --- | --- |
| `make test-lrz` | Only for changes to LRZ-gated code, backend feature wiring, or `Cargo.toml` feature definitions. |
| `make test-vct` | Only for changes under `vote-commitment-tree/` or `vote-commitment-tree-client/`. |
| `make doc-test` | Only when doc comments containing examples changed. |
| `make msrv` | Only when adding a dependency or using a newer language feature. |
| `make proofs` | Only for changes to circuit inputs or proving code. Very slow. |
| `make help` | List all targets. |

**Prefer the default Zakura path.** `make check` and then `make test` is the
normal cycle, and passing them is enough to consider a change verified unless it
touches one of the areas above. `zakura` is the default feature and the path
nearly all changes affect; the `lrz` variants build a second, entirely separate
crypto stack and roughly double the work for no added signal on Zakura-only
changes. CI runs every backend on every pull request, so skipping them locally
does not let a regression through. Do not run the LRZ or VCT suites
speculatively or "to be safe".

Each target pins its own `CARGO_TARGET_DIR` per feature permutation
(`target/zakura`, `target/lrz`, `target/vct`). This matters: the `zakura` and
`lrz` features are mutually exclusive and pull two separate crypto stacks, so a
bare `cargo` command that switches backends inside one target directory
recompiles the entire Halo2 dependency graph. `make check` and `make test` also
resolve to identical features, so they reuse each other's build artifacts.

Prefer `make check` over `make test` while iterating; it is much faster and
catches most errors.

Do not:

- run ad-hoc `cargo build` / `cargo test` / `cargo check` invocations — they use
  the shared default target directory and cause full-graph rebuilds;
- pass `--all-features` or a bare `--no-default-features`; CI asserts that both
  **fail**, because a wallet backend must be selected and the two backends are
  mutually exclusive;
- run the `#[ignore]` proof tests in debug. `zcash_voting/src/zkp1.rs` and
  `zcash_voting/src/delegation_capability.rs` hold three of them; they run
  Halo2 keygen plus proving, and one generates two real ZKP2 proofs. Use
  `make proofs`, which runs them in release.

`make pir-smoke` needs a sibling `../vote-nullifier-pir` checkout and is not
part of the test suite.

## Code standards

These rules apply to all Rust in this workspace. Apply them by default when
writing or editing code; they do not need confirmation. Before a refactor that
moves or renames anything, read the full standard in
[`.agents/skills/core-rust-readability/SKILL.md`](.agents/skills/core-rust-readability/SKILL.md),
which is the canonical text this section summarizes.

- **Name for the domain concept**, not the action or the underlying type. Reject
  `data`, `state`, `result`, `body`, `out`, `node`, `best` and similar
  placeholders when a specific name exists. Files and modules get discoverable
  domain names too.
- **Document public items and non-obvious methods** with purpose, validations,
  side effects, and postconditions or invariants. Explain domain terms and enum
  variants whose meaning is not self-evident.
- **Keep a thin facade with private children** rather than growing an overloaded
  multi-responsibility file. Split by responsibility or phase, not by line
  count. The facade shows entry points and phase order; children hide mechanism.
- **Keep each layer's API at its own abstraction level.** A low-level store
  should expose domain-neutral storage operations, not encode higher-level
  workflows. Prefer one cohesive typed transition API over several overlapping
  special-case methods; avoid `_inner` / `_with_conn` / `_for_<caller>` families.
- **Put tests in a `tests/` sibling directory** grouped by behavior with shared
  fixtures, not in an inline `#[cfg(test)] mod tests` appended to the production
  file.
- **Keep test-only fixtures and `#[cfg(test)]` branches out of production
  paths** when a black-box test or test helper can observe the same behavior.
- **Use distinct, accurate errors for distinct states**, so debugging does not
  require reading the implementation.

`zcash_voting/src/share_policy/` and `zcash_voting/src/share_tracking/` are the
in-repo examples of the target shape: a thin facade `mod.rs` holding the child
declarations, shared DTOs, and re-exports, with phase children beside it and
behavior-grouped tests in a `tests/` sibling. Read one of them before adding a
new module.

Several large files (`storage/operations.rs`, `storage/queries/mod.rs`,
`vote.rs`, `session.rs`, `config/mod.rs`, `delegate.rs`) predate this standard
and carry inline test modules and mixed responsibilities. They are legacy, not
the pattern; do not copy their layout into new code, and do not reformat them
wholesale as a side effect of an unrelated change.

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
