# Release branches and backports

`main` is the development line for the next release. A branch named
`release/vMAJOR.x` is the maintenance line for a shipped major series, such as
`release/v3.x` for the `v3` releases. The maintenance line exists so a fix can
ship against an already-released version without also shipping whatever else has
landed on `main` since.

Cut a maintenance branch from the chosen release commit before the first release
candidate on that line. Every release tag for the line should be reachable from
its branch: `v3.1.0-rc.17` and `v3.1.0` both belong on `release/v3.x`. Nothing
enforces this in CI, so check it before tagging.

Tags in this repository take two forms. `zcash_voting` releases are tagged
`v<version>`, and the other published crates are tagged `<package>-v<version>`,
such as `vote-commitment-tree-v0.6.0`. Publishing itself is manual and is
described in `.agents/skills/release-librustvoting/SKILL.md`; this document
covers only which branch a change lands on.

## What belongs on a maintenance line

A maintenance line ships bug fixes and other semver-compatible changes to
consumers already pinned to that major version. Before requesting a backport,
confirm the change does not:

- remove, rename, or narrow a public API, or change the meaning of one;
- add a public API that a later release on `main` would define differently;
- add a feature, rather than correct a defect;
- raise the MSRV, or take a major-version bump of a public dependency; or
- change a serialized or on-disk representation that an existing release reads,
  including the SQLite round-state schema and any wire representation.

Changes to tests, CI, documentation, and developer tooling are normally safe.
When a fix is genuinely needed but cannot be made compatibly, it belongs in the
next release from `main`, not on the maintenance line.

## Backport flow

Changes merge to `main` first. Apply `A:backport/v3.x` to the source PR when the
change should also ship on the maintenance line. After the source PR merges,
Mergify opens a separate PR against `release/v3.x` and assigns it to the source
author.

That backport PR is an ordinary PR. It runs the normal CI suite and needs human
review; it is never merged automatically. If the cherry-pick conflicts, Mergify
opens the PR anyway and applies `A:backport/conflict` — resolve the conflict on
the generated PR rather than pushing directly to the maintenance branch.

Release-only metadata, such as a version bump for the maintenance line, may
target `release/v3.x` directly. An emergency fix made directly on the branch
must be forwarded to `main` immediately afterward, or the next release will
silently regress it.

Applying a backport label never creates a tag, publishes a crate, or releases
anything. Releases remain explicit tags cut by a human.

## Retiring a line

When a maintenance line reaches end of life, remove its rule from
`.github/mergify.yml` and delete its `A:backport/*` label. Keep only the
currently supported release lines active.

## Requirements

The backport automation runs through the Mergify GitHub App, which must be
enabled for this repository by a `valargroup` organization administrator.
Until it is, `.github/mergify.yml` is inert: applying the label will have no
effect and backports must be cherry-picked by hand.
