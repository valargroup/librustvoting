# Recoverable voting authority design

## Status

This is a proposal for human review. It asks reviewers to approve the recovery
model and repository boundaries before implementation. Exact byte encodings,
test vectors, storage layouts, and API shapes belong in the implementation
work.

On-device derivation from the Keystone mnemonic remains outside this version.
The current Keystone path requires no firmware change.

## TL;DR

Each Zcash account gets a separate voting-authority root for each network, vote
chain, and round. Software wallets derive it from their seed. Current Keystone
hosts instead back up a random, versioned master generation scoped to one
account, network, and vote chain; `zcash_voting` derives and the wallet retains a
distinct root for each round. The root always recreates the voting hotkey. For
self-custodied funds, `zcash_voting` also recreates each bundle's VAN blinding
from the root and the canonical snapshot bundle plan. For funds in custody, the
existing capability package remains the source of bundle weights and blindings.
An authenticated round configuration selects this framework and the snapshot
used to reconstruct bundles. After data loss, the wallet can recreate its
authority, find its latest VANs in validated vote-chain data, and continue after
partial delegation or later votes. Existing rounds, circuits, vote-chain
messages, atomic batching, Keystone signing, and firmware remain unchanged.

## Problem

Delegation currently depends on randomly generated secrets:

- the voting hotkey, which is needed before delegation and authorizes later
  votes; and
- one `van_comm_rand` blinding for each Vote Authority Note (VAN).

If a delegation reaches the vote chain and those values are later lost,
restoring the Zcash account does not recreate the same VAN. This can prevent a
voter from using a confirmed delegation, especially when only part of a
multi-bundle delegation was submitted before local state was lost.

The recovery identity cannot be the transaction hash because rebuilding a
valid transaction can change unrelated transaction and proof randomness. The
wallet must be able to reconstruct the original VAN and locate it in validated
vote-chain state.

## Proposed model

`zcash_voting` defines one versioned authority framework with two independent
choices:

| Choice | Version 1 options |
| --- | --- |
| Authority-root source | Derive the round root from a software wallet seed, or from a selected backed-up Keystone host master generation |
| Bundle-material source | Derive blindings for self-custodied funds, or import the existing capability for funds in custody |

Both choices and, for a master-backed authority, the selected master generation
are recorded before the authority is used. They become immutable once a public
target is published or a delegation may have been submitted. They are local
recovery metadata and do not appear on either chain.

The framework has one stable boundary:

```text
software seed -----------------------------+
                                           |
backed-up Keystone master + round context --+--> voting-authority root
                                                        |
                                                        +--> voting hotkey
                                                        |
                                                        +--> self-custody bundle blindings

custody capability ------------------------------------------> custody bundle material
```

Every integration supplies a typed authority source and bundle source.
`zcash_voting` owns the canonical master-to-round derivation and, from the
per-round root onward, authority construction and recovery. Integrations must
not create their own parallel derivation.

### Authority context

Every per-round root and all of its outputs are bound to:

- the Zcash network;
- the ZIP-32 account index and a fingerprint of the actual Orchard account;
- the vote-chain ID; and
- the vote-round ID.

A Keystone host master generation is scoped to the first three items; deriving
a round root adds the vote-round ID. A generation is not shared across accounts,
networks, or vote chains.

The account fingerprint prevents the same account index from accidentally
selecting an authority belonging to another seed. It is derived from the
Orchard full viewing key and stays in local encrypted state.

### Software wallets

A software wallet derives a 64-byte authority root from its wallet seed using
the ZIP-32 registered-key construction. The derivation is hardened, bound to
the complete authority context, and separated from the wallet's spending-key
paths. The namespace is owned by this application and does not require a ZIP
number or ZIP publication before deployment.

The wallet-facing `zcash_voting` API does not accept the seed or an account
spending key. A narrow wallet-owned provider derives the root, verifies the
account fingerprint, and returns only the authority root.

The same seed, account, network, vote chain, and round therefore recreate the
same root. A UFVK or watch-only wallet can verify the account fingerprint but
cannot derive the private authority.

### Current Keystone integrations

Current Keystone firmware does not derive voting authority. The host creates a
random, versioned voting-master generation for each account, network, and vote
chain, stores it in platform secure storage, and maintains an authenticated
encrypted account authority backup independent of both platform secure storage
and `VotingDb`.
The backup durably preserves active and retired master generations and each
immutable round-to-generation, root, and source binding. Generation activation
and each round binding are committed and read back before use. `zcash_voting`
derives each round's authority root from the selected generation and complete
authority context. The wallet retains that root in encrypted authority state.

Each round records its selected generation before publishing a target or
allowing delegation. A failed or interrupted write is retried with the same
generation, root, and selection. Once an authority may have been used, the
round cannot switch generations. Rotation creates and backs up a new default
generation only for rounds whose authority has not been used. It neither
reassigns nor revokes the generation selected for a used round.
Generation creation, retirement, and the retained generation set are
rollback-protected. Restore rejects stale generation state and uses a retired
generation only for rounds already assigned to it. Older generations remain
available for recovery of those rounds.

A master generation by itself cannot delegate funds or complete a vote.
Delegation additionally requires the funds owner's Zcash account signature.
Completing a vote additionally requires either the self-custody viewing or
snapshot-note state needed to reconstruct the bundle, or the exact custody
capability. An attacker who obtains both the generation and that additional
material after delegation can derive the hotkey and vote. The master cannot
spend the Zcash account's funds.

The account authority backup is the recovery source. Restoring only the Keystone
mnemonic does not recreate it, and a missing generation must not be replaced for
a round already assigned to it. Stale or incomplete restored state fails closed.

Keystone continues to sign delegation PCZTs through its existing signing flow.
After a delegation is confirmed, the restored host-side authority signs votes;
the device is needed only if a remaining delegation transaction still needs a
Zcash account signature. Reconstructing self-custody bundles also requires the
original account's viewing data or equivalent restored snapshot-note state.

The provider boundary leaves room for a future firmware feature that returns
the same kind of per-round root. Such a feature would affect root custody only.
It would not change the hotkey expansion, bundle derivation, recovery flow,
circuits, or vote-chain messages, and it could not recreate an older
host-generated master.

### Source selection and backup

The account authority backup stores the selected authority source and bundle
source in authenticated encrypted state outside `VotingDb`. A master-backed
round also stores its generation identifier and retained per-round root. The
master secret alone does not replace this per-round selection record. A software
authority stores the same selection without a master generation.

This record prevents a restored wallet from silently switching between a seed
and a Keystone master, or between master generations. If the record is lost,
recovery may reconstruct the choice only when authenticated external evidence
identifies exactly one matching authority. Ambiguous or conflicting evidence
fails closed.

## Canonical authority construction

### Voting hotkey

`zcash_voting` derives one Orchard voting spending key directly from the
authority root and context using a versioned, domain-separated expansion. The
result is a voting-only key. It cannot spend the Zcash account that supplied
the delegation weight.

The expansion is identical for software-derived and master-derived per-round
roots. The root source therefore remains a custody concern rather than a second
protocol implementation.

Master generations, version 1 per-round roots, and existing random 64-byte
hotkey secrets have distinct typed meanings and are never inferred by length.

### Self-custodied bundle material

For self-custodied funds, the restored wallet must reconstruct both the
blinding and the bundle weight. `zcash_voting` therefore owns a versioned,
canonical snapshot-note and bundle policy.

Version 1 freezes the current default bundle behavior and makes the previously
implicit inputs deterministic:

- select the original account's eligible Orchard notes at the authenticated
  snapshot, including notes spent after that snapshot;
- order and deduplicate notes canonically;
- apply the existing bundle packing, zero-ballot, and privacy-drop rules; and
- identify each surviving bundle from its index and real notes, including each
  note's position, commitment, and value.

Random padding, proofs, transaction fields, and transaction hashes are not
part of bundle identity. Each bundle receives a domain-separated VAN blinding
derived from the authority root, authority context, and canonical bundle
identity.

Changing the note policy or bundle identity requires a new version. Manual note
selection or a custom bundle policy is recoverable only if it carries its own
complete recovery description; otherwise the wallet must present it as
non-recoverable.

An intentionally unsubmitted suffix of the canonical bundle plan does not
change the identity of earlier bundles. This lets recovery determine which
prefix reached the vote chain and continue with bundles that were never
submitted.

### Funds in custody

For funds in custody, the existing public-target flow stays intact:

- the voter derives the hotkey from its authority root and publishes the
  existing round-bound target;
- the funds controller applies the canonical bundle policy to its own notes,
  samples each VAN blinding, and constructs the existing delegation
  capability; and
- the voter imports that capability through the typed `zcash_voting` API.

The root recovers the voting hotkey. The exact capability recovers the bundle
indices, weights, blindings, and delegation transaction identities. The voter
retains an encrypted copy and the funds controller keeps its existing
redeliverable copy. If both are lost, the hotkey remains recoverable but the
custody bundle material is not.

For a master-derived root, a matching capability is enough to bind the restored
authority even if the voter's original Orchard viewing key is no longer
available. This does not allow the same root to reconstruct self-custodied
bundles.

No new custody exchange, circuit input, or vote-chain field is required.

## Authenticated round selection

The new authority is indistinguishable from the legacy random authority on the
vote chain. The wallet therefore needs an authenticated instruction telling it
which construction to use. Without that instruction, two wallets could process
the same round with different authority rules and produce authorities that
cannot be recovered consistently.

A new authenticated round-config version binds:

- `recoverable-authority-v1` as the authority scheme;
- `recoverable-v1` as the bundle policy; and
- the Zcash snapshot height and block hash.

The height tells wallets which snapshot to use. The hash ensures they use the
same fork at that height. Construction and recovery require the wallet scan,
tree state, and PIR snapshot metadata to match that authenticated pair.

One round ID has one immutable authenticated payload. Changing the snapshot,
scheme, policy, election key, or PIR parameters requires a new round ID.
Existing authenticated rounds remain legacy `random-v0`; an older wallet that
does not understand the new config simply does not join the new round.

Because this round-config version does not sign the Zcash network or vote-chain
ID, its trusted signing keys and configuration namespace are scoped to exactly
one network and one vote chain. Reusing those keys across either boundary
requires a later version that signs both identifiers.

This is a client-configuration change, not a consensus or vote-action change.

## Recovery

After restoring the authority selection, the wallet:

1. obtains the same root from the software seed or the master generation
   selected for that round, and verifies any retained root matches;
2. reconstructs self-custody bundles from the authenticated snapshot or
   restores custody bundles from the exact capability;
3. recreates each initial VAN;
4. finds that VAN in a root-validated vote-commitment tree; and
5. follows confirmed singleton or atomic vote transitions until it reaches the
   latest VAN and proposal-authority mask.

The wallet starts each recovery attempt from an independently authenticated
finalized vote-chain checkpoint. The tree and transition data must be complete
through that same height and block hash. An endpoint's own tip is not enough,
because recovery uses absence of a transition as evidence that the current VAN
is still unspent.

Every transition is recomputed from the recovered authority and checked against
the validated tree. The current 16-proposal mask bounds recovery to at most 15
successor transitions per bundle. Missing, conflicting, incomplete, or
ambiguous data fails closed instead of probing the chain by attempting another
vote.

This recovery needs complete public transition data, but not the wallet's
previous transaction database, confirmed vote history, or prior ballot choices.
If the user tries to vote a proposal that was already consumed, normal
validation rejects it.

Failure to find an initial VAN is not by itself permission to delegate again.
Ordinary transaction reconciliation must first establish that the delegation
was never confirmed and that its source notes remain usable.

### Votes awaiting helper completion

Authority recovery does not by itself restore a vote that was broadcast while
helper-share delivery was still incomplete. During that interval,
`zcash_voting` exports and imports a versioned backup of the complete pending
vote or ordered atomic batch and its existing helper-tracker state. Multiple
pending votes may coexist; updating or retiring one must not discard or
resurrect another.

The wallet durably stores the required state before each vote or helper POST
that depends on it. Because the backup contains ballot secrets, it must be
encrypted, authenticated, crash-safe, and able to detect stale rollback. The
storage layout and atomic-update mechanism are implementation details.

Restore validates the authority, round, bundle, batch, confirmed tree
positions, complete original helper plans, and delivery identities before
resuming the existing tracker. It preserves durable schedules, targets, and
accepted, ambiguous, and interrupted evidence, and it permits no helper POST
without a real confirmed tree position. Helper behavior continues to follow
the [helper submission invariants](helper_submission_invariants.md).

The backup remains available until an unsubmitted intent is validly replaced
or the tracker marks every expected share confirmed. Ordinary recovery cleanup
must not remove the last restorable copy while helper completion is pending.
Retirement must not allow an older backup to reappear as live state. Complete
confirmed vote history is not retained.

## Batching

The derivations are keyed to the authority and delegation bundle, not the
transaction container. The current atomic vote batch carries ordered actions
for one bundle and therefore works unchanged. It shares that bundle's hotkey
and VAN transition chain.

If a future transaction carries actions from multiple bundles, each bundle
keeps its own blinding and VAN chain. Atomic transaction construction does not
create a batch-wide authority or `van_comm_rand`.

Keystone PCZT batch signing also works unchanged because it signs transactions
after their authorities and delegation bundles have already been constructed.

## Compatibility and boundaries

- Existing rounds and persisted authorities remain `random-v0`. Their stored
  hotkey secret and VAN blindings remain authoritative.
- Version 1 cannot recreate random secrets already lost from a legacy round or
  change an existing on-chain VAN to a new authority.
- No voting-circuit, proof statement, vote-chain message, or consensus change
  is required.
- No Keystone firmware, key export, QR type, or on-device vote operation is
  required.
- A future firmware-derived Keystone provider remains possible through the
  common per-round-root boundary.
- A UFVK or watch-only wallet is not sufficient to recover private authority.
- Funds in custody remain dependent on the retained or redelivered capability.
- Custom note selection remains outside the recoverable path unless it carries
  a complete, versioned recovery description.

## Ownership

| Component | Responsibility |
| --- | --- |
| `zcash_voting` | Versioned context and source types; master-to-round-root, hotkey, and self-custody bundle derivation; custody capability validation; round selection; VAN recovery; pending-vote export/import; shared vectors and fixtures |
| Wallet integration | Seed or master provider; master creation and rotation; authenticated round-to-generation storage; retained per-round roots; encrypted backups; snapshot rescan; independently authenticated recovery checkpoint; recovery UX |
| Current Keystone integration | Master-backup gate, account binding, immutable generation selection, and existing PCZT or PCZT-batch signing |
| Keystone firmware | No version 1 change; a future root provider may use the same boundary |
| `vote-sdk` | New authenticated round fields and signing support; complete block-bound transition data for recovery |
| `vote-nullifier-pir` | Publish snapshot height and block hash for the exact dataset |
| `voting-circuits` and vote chain | No protocol change; accept the same private witnesses and public statements as today |

## Decision requested

Reviewers are being asked to approve these directions:

1. one versioned per-round authority-root boundary and canonical master-to-round
   derivation owned by `zcash_voting`;
2. deterministic software roots and backed-up host master generations scoped
   to one account, Zcash network, and vote chain, with retained per-round roots
   for current Keystone integrations;
3. root-derived bundle blindings for self-custodied funds while preserving the
   existing capability flow for funds in custody;
4. a canonical, versioned snapshot bundle policy;
5. authenticated round selection including the snapshot height and block hash;
6. recovery by validated VAN transition traversal rather than saved transaction
   history;
7. reuse of the existing helper tracker for votes awaiting helper completion;
8. future-only master rotation with immutable generation selection for used
   rounds; and
9. no circuit, vote-chain, batching, or Keystone firmware change in version 1.

Once these decisions are approved, implementation work can define and review
the exact encodings, APIs, storage adapters, and cross-repository test vectors.
