# Recoverable voting authority design

## Status

This document is a proposal for review. It does not describe behavior that is
implemented today. The review goal is to approve the authority model,
derivation boundaries, backup contract, recovery behavior, batching
compatibility, and migration boundary before code is written. Deterministic
Keystone hotkey derivation is explicitly outside this version. The version 1
Keystone path uses the firmware and transaction-signing flow that exist today.

## TL;DR

For each account and voting round, the voter creates one voting-authority root:
software wallets derive it from their seed, while current Keystone wallets
generate and securely back it up without requiring firmware changes. The root
always recreates the voting hotkey. For self-custodied funds, `zcash_voting`
also derives each bundle's VAN blinding from the root and canonical bundle
plan. For funds in custody, it instead restores bundle blindings and weights
from the existing capability package retained by the voter and funds
controller. The signed round configuration selects this common version 1
framework and authenticates the snapshot height used to reconstruct its bundle
plan. After data loss, the restored root and bundle material let the
wallet find its latest VANs in the validated vote tree and continue after
partial delegation or singleton and atomic votes. While helper-share delivery
is incomplete, a separate encrypted pending-tally record keeps that vote
recoverable if the voting database is lost. Existing rounds remain unchanged.

## Executive summary

Creating a delegation currently requires two kinds of randomly generated
secret material, both of which must remain available afterward:

- one voting hotkey, which authorizes later votes; and
- one `van_comm_rand` blinding factor for each Vote Authority Note (VAN).
  Some existing code calls the same value `gov_comm_rand`.

If a delegation reaches the vote chain and the wallet later loses either
secret, restoring the Zcash wallet is not enough to recreate that VAN. This is
especially harmful when only part of a multi-bundle delegation was submitted
or confirmed before local state was lost.

Version 1 introduces one 64-byte **voting authority root** for each voter
account, network, vote chain, and round. `zcash_voting` always expands that root
into one Orchard voting hotkey.

Each authority then gets its complete bundle set from one typed source:

- `derived-v1` deterministically derives an independent VAN blinding from the
  root and canonical bundle identity when the voter also controls the funds;
  or
- `imported-capability-v1` obtains the bundle weight and randomly sampled VAN
  blinding from the existing public-target custody capability package.

The root can come from more than one source without changing the hotkey
expansion:

- a software wallet derives it from its wallet seed; or
- an integration that cannot derive it from its seed, including today's
  Keystone integrations, generates it randomly and makes a durable encrypted
  backup before delegation can be submitted.

```text
Software wallet seed ---- software seed provider ---+
                                                     |
Stored random root ------- encrypted backup --------+--> authority root v1
                                                               |
                                                               +--> voting hotkey
                                                               |
Canonical local bundles -----------------------------+--> derived VAN blinds
Custody capability -------------------------------------> imported VAN blinds
```

For a current Keystone wallet, the hotkey is therefore not recoverable from the
Keystone mnemonic. It is recoverable from the separate voting-authority backup.
Keystone continues to sign the completed delegation PCZT or PCZT batch exactly
as it does today; no new firmware operation or QR type is required.

The root-provider boundary is intentional. A future Keystone firmware feature
can derive and return the same kind of root. The hotkey expansion, local bundle
derivation, imported-capability path, recovery state machine, circuits, and
vote-chain messages would not need to change.

The authority construction is client-side. The voting circuits and vote-chain
verifier do not need to know whether a private witness was sampled or derived.
After restoration, the root recreates the hotkey, while the selected bundle
source either re-derives or restores the bundle blindings. The wallet can then
find its matching VANs in the validated vote tree and continue voting.

### Recovery at a glance

| Recovery case | Recovery material | Result |
| --- | --- | --- |
| Software seed provider | Original wallet seed or mnemonic, authority-selection marker or uniquely matching external use, and a rescan of the original account | Re-derive the same authority root and canonical bundle plan |
| Current Keystone integration with a confirmed delegation | Encrypted voting-authority backup plus the original account's Orchard viewing capability or equivalent restored snapshot-note state | Restore the root and canonical bundle plan, then vote without a Keystone signature or device operation |
| Current Keystone integration with remaining delegation transactions | Encrypted voting-authority backup plus access to the original Zcash account, normally through Keystone | Restore the root, then sign the remaining delegation PCZTs |
| Public-target custody with retained capability | Voter's seed or encrypted authority-root backup plus the exact custody capability package | Re-derive the hotkey, import the original bundle weights and blindings, then recover matching VANs |
| Public-target custody without either retained or redeliverable capability | No bundle recovery | The root recreates the hotkey but not custody-provider blindings |
| Confirmed vote with helper shares still incomplete | Encrypted pending-tally record | Restore the exact vote recovery bundle and resume helper delivery |
| Keystone wallet or mnemonic without the voting-authority backup | No recovery | The Keystone seed does not contain the randomly generated root |
| UFVK or watch-only wallet | No recovery by itself | Public keys cannot reveal voting authority |
| Legacy `random-v0` round | Original hotkey and every original bundle blinding | Version 1 cannot recreate already-lost random values |

The viewing capability in the confirmed-Keystone row may already be present in
the wallet's normal encrypted backup. In that case the Keystone device is not
needed. For direct delegation, the authority backup by itself can re-derive the
hotkey but cannot reconstruct bundle IDs or weights without the original
account's snapshot notes. A custody voter uses the capability package instead
of the funds controller's snapshot notes.

## Motivating failure

Consider a voter whose snapshot notes produce three delegation bundles:

1. Bundle 0 is signed and confirmed on the vote chain.
2. The app crashes or its voting database is lost before all local recovery
   state is durable.
3. The user restores the same Zcash account.

Today the restored app samples a different hotkey and a different VAN blinding
factor. It therefore computes a different VAN for bundle 0. Rebuilding the
delegation transaction does not help because the original eligible notes may
already be represented by the confirmed delegation, and later vote proofs must
use the hotkey and blinding factor committed by that original VAN.

A transaction hash is not a durable identity for this recovery. Other valid
transaction and proof randomness can change when work is rebuilt. Recovery
must recompute the original VAN and locate that commitment in a root-validated
vote tree.

Public-target custody has a different recovery input: its existing capability
already carries the controller-generated weights and blindings. Version 1 keeps
that contract and makes the voter hotkey recoverable from the root instead of
requiring the controller's bundle secrets to become root-derived.

## Goals

- Restore the same software-wallet voting hotkey from the same wallet seed,
  account, network, vote chain, and round.
- Restore the same current-Keystone voting hotkey from a durable encrypted
  voting-authority backup, without changing Keystone firmware.
- Restore a different, deterministic VAN blinding factor for every locally
  controlled canonical delegation bundle.
- Preserve public-target custody by importing its existing capability package,
  without adding another exchange or changing delegation construction.
- Give every integration one canonical `zcash_voting` authority-material API
  after the root and typed bundle source have been supplied.
- Keep the wallet root seed, account spending key, and registered-key subtrees
  out of `zcash_voting`.
- Preserve one fresh, unlinkable voting authority per account and round.
- Recover confirmed partial delegation from VAN commitments rather than exact
  transaction bytes.
- Recover the latest confirmed VAN state after singleton or atomic voting
  without requiring the wallet's prior transaction history.
- Resume incomplete helper-share delivery after voting-database loss without
  retaining complete confirmed vote history.
- Work unchanged with atomic vote batches and Keystone PCZT batch signing.
- Version every persisted derivation artifact so legacy random rounds continue
  to work without reinterpretation.
- Consolidate the encoding, expansion, bundle identity, recovery, and test
  vectors in `zcash_voting`.
- Preserve a narrow provider seam for a future on-device Keystone root without
  making that firmware work part of version 1.

## Non-goals

- Derive a deterministic hotkey or authority root from a Keystone seed in
  version 1.
- Add a Keystone firmware operation, private-key export, UR type, or encrypted
  QR response in version 1.
- Claim that a Keystone mnemonic restores a `stored-random-v1` authority.
- Rotate or replace a `stored-random-v1` authority after its backup record is
  created.
- Recover random secrets that were already lost in a legacy round.
- Change the ZKP statements, on-chain messages, or vote-chain verifier.
- Move ZKP generation or vote signing into Keystone.
- Make an imported UFVK sufficient to reconstruct private voting authority.
- Recover an earlier ballot after all of its helper shares are confirmed, or
  reproduce the exact outer transaction bytes after the local voting database
  is lost.
- Guarantee recovery for a manually selected note subset or custom bundle
  policy that is not itself recoverably described.
- Make public-target custody recovery root-only. Its bundle material still
  requires the retained or redelivered capability package.
- Specify the transport or user interface of a possible future Keystone root
  provider.

## Terms

### Account fingerprint

The stable identifier that binds an authority to the actual Orchard account,
not merely to an account index that many unrelated seeds may share:

```text
account_fingerprint_32 = BLAKE2b-256(
    personalization = "ZcashVoteAcctV1",
    message = orchard_full_viewing_key_96,
)
```

The ASCII personalization is copied into BLAKE2b's 16-byte personalization
field and zero-padded on the right. `orchard_full_viewing_key_96` is the
canonical 96-byte encoding returned by Orchard `FullViewingKey::to_bytes`. A
software provider derives it from the same seed and account path. A current
Keystone integration extracts it from the account UFVK supplied during wallet
pairing. An imported UFVK can validate the fingerprint but still cannot derive
the private authority root.

The fingerprint is not secret, but it is account-linking metadata. It remains
inside local state and encrypted backups and is not published on either chain.

### Voting authority context

The canonical voter-account and round identity used by the version 1 hotkey and
locally controlled bundle expansions:

```text
authority_context_v1 =
    0x01
    || network_tag_u8
    || account_index_u32_le
    || account_fingerprint_32
    || vote_chain_id_length_u16_le
    || vote_chain_id_utf8
    || vote_round_id_32
```

`network_tag_u8` is the protocol byte `0` for mainnet, `1` for testnet, and `2` for
regtest. Implementations map by network name, not an internal enum discriminant.
`account_fingerprint_32` must match the account selected by
`account_index_u32_le`; a provider rejects a mismatch before returning a root.
`vote_chain_id` must first pass the crate's canonical chain-ID validation.
`vote_round_id_32` is the canonical little-endian 32-byte Pallas base-field
encoding used by the round. The ZIP-32 account index must be below `2^31`.

The encoding is prefix-free because variable-length fields carry an explicit
length and all remaining fields have fixed lengths.

### Voting authority root

An opaque 64-byte secret supplied to `zcash_voting` for one authority context.
The root is the stable boundary between secret custody and voting protocol
logic. It always recreates the hotkey and, for `derived-v1` bundles, the VAN
blindings. It cannot spend the Zcash account.

### Authority root source

The local method used to obtain a root. Version 1 defines
`registered-seed-v1` and `stored-random-v1`. The source is recovery metadata; it
does not alter root expansion or appear on-chain.

### Bundle material source

The typed local source of an authority's complete bundle set. Version 1 defines
`derived-v1` for locally controlled bundles and `imported-capability-v1` for the
existing public-target custody package. One source is selected per authority;
version 1 does not mix locally constructed and imported bundles for the same
account and round. The source is persisted recovery metadata and does not
appear on-chain.

### Authority selection marker

The immutable, nonsecret record that binds an authority context to its root and
bundle source tags outside `VotingDb`. A software authority stores a
`VotingAuthoritySelectionV1`; a stored-random authority uses its
`VotingAuthorityBackupV1`, which carries the same selection plus the secret
root. The marker is still account-linking metadata and belongs in authenticated
encrypted backup storage.

### Voting hotkey

An Orchard `SpendingKey` used only by the shielded-voting protocol. Its address
is the delegation target and its spend-authorizing key signs later vote-chain
actions. It cannot spend the Zcash account that delegated the voting weight.

### VAN blinding factor

The Pallas base-field element used to hide the address and weight in a VAN
commitment. The storage and delegation code call it `van_comm_rand`. Some vote
builder APIs call the same value `gov_comm_rand`; version 1 should use the VAN
name consistently at new API boundaries.

## Design decisions

### 1. Standardize one authority framework and root boundary

`zcash_voting` owns these concepts:

```text
VotingAuthorityContextV1
VotingAuthorityRootV1(64 bytes)
AuthorityRootSourceV1
BundleMaterialSourceV1
VotingAuthoritySelectionV1
```

A root provider accepts the canonical context and returns exactly one 64-byte
root. Every provider uses the same `zcash_voting` hotkey expansion. A direct
delegation selects `derived-v1`, so `zcash_voting` also owns its bundle ID and
VAN-blinding derivation. A public-target import selects
`imported-capability-v1`, so the existing validated capability is the canonical
source of bundle weights and blindings. Integrations must not invent another
untagged source.

Before returning a root, the provider validates that the context's account
fingerprint matches the Orchard full viewing key for the selected network and
account index. This prevents a backup or randomly generated root belonging to
another seed's account at the same index from being accepted as the current
authority.

The provider and bundle sources are selected before direct delegation or
publication of a public target. `registered-seed-v1` persists this canonical
selection plaintext outside `VotingDb`:

```text
{"format_version":1,"root_source":"registered-seed-v1","bundle_source":"<derived-v1 or imported-capability-v1>","authority_context":"<Base64>"}
```

It uses the same compact JSON, field-order, canonical padded Base64, strict
parsing, and context validation rules as `VotingAuthorityBackupV1` below. The
integration commits and reads back the authenticated encrypted record before
publishing a public target or allowing direct delegation submission. A
`stored-random-v1` backup is the corresponding source marker for that path, so
it does not need a second record.

The first marker for an authority context is final. Copies and re-encryption
must preserve its canonical plaintext, and a restore path without the marker
must not infer the selection from application version, secret length, device
type, or whichever recovery input is available. Once a public target is
published or any direct delegation may have been submitted, both sources, the
context, and the root are immutable.

The root source is intentionally not included in the expansion. If two
conforming providers return the same root for the same context, they produce
the same hotkey and, for `derived-v1`, the same bundle blindings. This is what
allows a future Keystone provider to join without another downstream
derivation scheme.

### 2. Derive software-wallet roots from the wallet seed

A software wallet uses `zip32::registered::cryptovalue_from_subpath`, the
hardened primitive from the [ZIP 32 registered-key construction][zip32-registered],
under the ASCII context `ZcashShieldedVoting` and this fixed numeric namespace:

```text
VOTING_KDF_ID_V1: u16 = 0x5654 = 22,100
```

The hexadecimal value spells `VT` in big-endian byte order and fits the
canonical API's `u16 zip_number` argument. It is passed as that argument to
`RegKD`, whose subtree step therefore uses hardened child index
`VOTING_KDF_ID_V1 + 2^31 = 0x80005654`.

Neither value requires external assignment or a standalone ZIP before
deployment. `zcash_voting` defines them as version 1 protocol constants. This is
an application-defined use of the primitive, not a claim that ZIP 22100 exists.
Changing either value requires a new root-source version; a later ZIP may
document the deployed values without renumbering version 1.

Conceptually, the tree is:

```text
RegKD("ZcashShieldedVoting", wallet_seed, VOTING_KDF_ID_V1)
    / coin_type(network)'
    / account_index'
    / authority-root=0'(tag = authority_context_v1)
```

The coin-type and account elements use empty tags. `coin_type(network)` is 133
for mainnet and 1 for testnet or regtest, matching the account path used by the
wallet. The account-level registered key is never exported. The final operation
uses `derive_child_cryptovalue` at hardened child index zero to produce the
64-byte `VotingAuthorityRootV1`.

The software provider derives the account's Orchard full viewing key from the
same seed, coin type, and account index and verifies
`account_fingerprint_32` before deriving the registered cryptovalue. A context
copied from another account therefore fails before any authority root is
returned.

The wallet-facing library API does not accept a mnemonic, root seed, account
spending key, or registered subtree key. A narrow software seed-provider helper
may live outside the ordinary API so integrations do not duplicate the path.

The software provider should derive the authority root when needed. If it
caches the root or derived hotkey, the cache belongs in platform secure storage,
not `VotingDb`. The wallet seed remains the independent recovery source.

The registered cryptovalue is not passed into any ZIP-32 KDF. The next section
defines a direct Orchard-key expansion instead.

### 3. Back up randomly generated roots before use

Today's Keystone firmware cannot supply the registered seed-derived root. A
Keystone integration therefore uses `stored-random-v1`:

1. Ask `zcash_voting` to generate 64 bytes with the operating system CSPRNG.
2. Select the authority's typed bundle source.
3. Bind the root, both sources, and canonical context in a typed backup record.
4. Store the live copy in platform secure storage.
5. Commit an authenticated, encrypted backup that can survive deletion or loss
   of the app's voting database.
6. Read back and validate the backup before publishing a public target or
   allowing any direct delegation submission.

The canonical plaintext payload is `VotingAuthorityBackupV1`. It contains the
format version, `stored-random-v1` root-source identifier, selected bundle
source, complete authority context, and 64-byte root. `zcash_voting` owns this
payload and its validation; the wallet integration owns encryption,
authentication, storage, and restore UX. The plaintext payload must never be
logged, placed in a transaction memo, or included in diagnostics.

Its exact plaintext encoding is compact UTF-8 JSON with these fields in this
order and no whitespace or trailing newline:

```text
{"format_version":1,"root_source":"stored-random-v1","bundle_source":"<derived-v1 or imported-capability-v1>","authority_context":"<Base64>","authority_root":"<Base64>"}
```

Both byte strings use canonical padded standard Base64. `authority_context` is
the complete `authority_context_v1` byte encoding, and `authority_root` must
decode to exactly 64 bytes. Parsing follows the existing capability-codec rule:
unknown, duplicate, missing, or reordered fields; whitespace; a trailing
newline; noncanonical Base64; an unknown source; or an invalid context is
rejected. A parser can enforce this by validating the decoded object,
serializing it canonically, and requiring byte-for-byte equality with the
input.

The root, both sources, and context become final when the wallet creates the
first backup record for that authority context. It must never create another
`VotingAuthorityBackupV1` for the same context with a different selection. A
failed or interrupted store is retried with the same canonical plaintext.
Additional backup copies and later re-encryption are allowed only when the
decoded plaintext remains byte-for-byte identical.

The read-back gate requires the decoded selection to equal that final
plaintext. This single-assignment rule means any authenticated backup for the
context restores the same authority, without relying on the deleted voting
database to identify a newer copy. Restore still checks the account fingerprint
below. If the final root is lost before the gate succeeds, version 1 cannot use
or replace that authority.

This public fixture freezes the encoding. It uses regtest, account index zero,
fingerprint bytes `00` through `1f`, vote chain `vote-chain-1`, little-endian
round field element 1, root bytes `00` through `3f`, and `derived-v1`:

```text
{"format_version":1,"root_source":"stored-random-v1","bundle_source":"derived-v1","authority_context":"AQIAAAAAAAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8MAHZvdGUtY2hhaW4tMQEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","authority_root":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+Pw=="}
```

The SHA-256 digest of those 325 plaintext bytes is
`1446b608217b821ecd4dd394c488a3f65ed7a0e723c4d10b28640bbdae2157ef`.

Because the complete context includes `account_fingerprint_32`, restore must
compare the record with the currently selected Orchard full viewing key before
making the root available. Matching network, account index, chain, and round is
not sufficient by itself.

Acceptable storage can include an integration's existing end-to-end encrypted
wallet backup or an explicit encrypted export. A second row in the same voting
database is not a backup. A device-only Keychain entry is useful live storage
but is not sufficient by itself if it disappears with the device or app.

If the integration cannot complete and verify the durable backup, it must not
label the authority recoverable, publish its public target, or submit a direct
delegation under `recoverable-authority-v1`. This is a real safety gate: after a
delegation reaches the vote chain, no different root can take over its
authority.

For `derived-v1`, the backup replaces the need to preserve every bundle
blinding separately. It does not contain the Zcash account's snapshot notes or
viewing capability. A confirmed direct delegation can be exercised without a
new Keystone signature once both that account data and the voting-authority
root are restored. `imported-capability-v1` additionally requires its custody
capability package. Remaining delegation transactions need access to the Zcash
account signer.

### 4. Derive the hotkey directly from the authority root

Version 1 derives a direct Orchard spending key:

```text
candidate(i) = BLAKE2b-256(
    key = authority_root_64,
    personalization = "ZcashVoteHotkey",
    message = authority_context_v1 || i_u32_le,
)
```

The ASCII personalization is copied into BLAKE2b's 16-byte personalization
field and zero-padded on the right. Starting with `i = 0`, interpret
`candidate(i)` with `Orchard::SpendingKey::from_bytes`. Increment `i` until the
result is valid. The expected number of iterations is one; the counter makes
the function interoperable for a rare invalid candidate. Return a derivation
error rather than wrapping if the `u32` counter is exhausted.

The hotkey uses external Orchard address index zero. APIs represent the legacy
hotkey seed and version 1 root as distinct typed variants; they never infer an
untagged secret's meaning from its length.

Implementations may cache the derived 32-byte Orchard spending key only in the
same platform secure-storage boundary as the root; neither secret belongs in
`VotingDb`. The root remains the recovery source. In-memory copies of both
secrets are zeroized after use.

This direct expansion deliberately differs from today's
`VotingHotkey::from_stored_secret`, which treats a random 64-byte value as a
ZIP-32 wallet seed. Existing rounds retain that interpretation as
`random-v0`; new version 1 roots have one uniform meaning regardless of source.

### 5. Use one typed source for bundle material

A round can have multiple delegation bundles that share one hotkey. Each bundle
must receive independent blinding material. One bundle source is selected by a
typed API before delegation or publication of a public target and persisted
with the authority. Version 1 does not mix sources within that authority.

#### Locally controlled bundles

`derived-v1` binds each blinding to a canonical bundle identity. Bundle index
alone is not enough: a policy change could assign different notes to the same
index after a database loss.

`zcash_voting` defines `VotingBundleIdV1` before any random padding notes, PCZT
fields, proofs, batch digest, or transaction bytes are created:

```text
bundle_id = BLAKE2b-256(
    personalization = "ZcashVoteBundle",
    message =
        0x01
        || bundle_index_u32_le
        || real_note_count_u32_le
        || for each note in canonical bundle order:
               position_u64_le
            || note_commitment_32
            || value_zatoshi_u64_le,
)
```

The ASCII personalization is copied into BLAKE2b's 16-byte personalization
field and zero-padded on the right. The note commitment must be its canonical
32-byte encoding. Canonical bundle order is the order returned by the version 1
snapshot-selection and bundle-planning policy. Padding notes are excluded
because their randomness is not recoverable and they do not define real voting
weight.

`bundle_index` is the zero-based index in the surviving bundle order after the
zero-ballot and privacy drops.

The blinding factor is then:

```text
wide = BLAKE2b-512(
    key = authority_root_64,
    personalization = "ZcashVoteVANv1",
    message = authority_context_v1 || bundle_id,
)

van_comm_rand = PallasBase::from_uniform_bytes(wide)
```

The ASCII personalization is copied into BLAKE2b's 16-byte personalization
field and zero-padded on the right. `from_uniform_bytes` avoids biased modular
reduction and always produces a canonical Pallas base-field element.

The authority root, rather than the derived hotkey bytes, is the KDF key. The
hotkey and bundle blindings are sibling outputs with separate domains. This
keeps the bundle derivation stable if a future provider changes how it obtains
the root and avoids using a signing key as a general-purpose KDF key.

The bundle ID may be persisted for diagnostics and conflict checks, but it is
privacy-sensitive metadata and is not published on-chain.

#### Public-target custody bundles

`imported-capability-v1` preserves the existing role-separated flow. The funds
controller samples each `van_comm_rand`, uses it while constructing ZKP #1, and
includes it with the bundle index, weight, and delegation transaction hash in
the existing `DelegationCapabilityV1`. No bundle-ID or blinding request is added
to the public-target handoff.

The voter derives the hotkey from its authority root, validates that the
capability target matches that hotkey and the authenticated round context, and
imports the package through the existing typed API. The voter durably retains
the exact package, while the controller keeps its existing redeliverable outbox
copy. Losing both copies loses custody bundle recovery even though the root can
still recreate the hotkey.

After construction or import, both bundle sources expose the same validated
weight, blinding, initial VAN, and current-VAN recovery interface. The
distinction does not reach the circuit or vote chain.

### 6. Freeze the recoverable note-selection policy

For locally controlled funds, deterministic secrets are insufficient if a
restored wallet cannot reconstruct the original bundle weights. Version 1
therefore uses a named, frozen snapshot-note selection and `BundlePolicy`
definition owned by `zcash_voting`.

The authenticated round configuration identifies:

- `voting_authority_scheme = "recoverable-authority-v1"`; and
- `bundle_policy = "recoverable-v1"`.

`recoverable-v1` requires the wallet to be scanned through the round snapshot
and selects notes from the originally selected account that meet all of these
conditions at that height:

- the account has an Orchard full viewing key, and the note belongs to either
  its external or internal scope;
- the note uses the shielded-voting pool, protocol, and note version active at
  the snapshot;
- the note was mined at or before the snapshot and was unspent at the snapshot,
  including a note that was spent later; and
- its commitment-tree position, canonical 32-byte commitment, nullifier, and
  value can be reconstructed.

Notes from another account, pool, or note version, notes mined after the
snapshot, and notes already spent at the snapshot are excluded. Before
planning, candidate notes are ordered by commitment-tree position with note
commitment as a tiebreaker. Duplicate nullifiers are then collapsed by keeping
the first note in that order.

The policy applies these frozen planning values:

| Property | `recoverable-v1` value |
| --- | --- |
| Real notes per bundle | 5 |
| Bundle-addition value threshold | Disabled |
| Zero-ballot bundle rule | Drop totals below `BALLOT_DIVISOR` (12,500,000 zatoshi) |
| Packing order | Value descending, then position ascending |
| Notes within a surviving bundle | Position ascending |
| Surviving bundle order | Total value descending, then minimum position ascending |
| Privacy bundle target | 2 |
| Privacy drop budget | Floor of 1% of selected note value |
| Absolute privacy drop ceiling | 100,000,000,000 zatoshi (1,000 ZEC) |

Let `selected_value` be the sum of the deduplicated candidate-note values before
zero-ballot bundles or privacy bundles are dropped. Using wide integer
arithmetic, the exact budget is:

```text
percentage_budget = floor(selected_value * 100 / 10_000)
effective_budget = min(percentage_budget, 100_000_000_000)
```

After zero-ballot bundles are removed and the surviving bundles are put in the
table's order, privacy trimming repeatedly removes the trailing bundle while
more than two bundles remain and
`already_dropped + trailing_bundle_value <= effective_budget`. These rules
match the current default policy except that version 1 also freezes the initial
note order and duplicate choice. Any change to the candidate predicate,
ordering, arithmetic, comparison, or constant requires a new policy identifier;
it must not silently change `recoverable-v1`.

Wallets may continue to support manual note subsets or custom policies, but
those flows must either carry their own recoverable selection descriptor or
tell the user that voting recovery is unavailable. They must not be labelled
`recoverable-v1`.

In public-target custody, the funds controller applies this same policy to its
account. The voter imports the resulting weights and blindings from the
capability package instead of reconstructing the controller's private note
plan.

An intentionally skipped suffix of the canonical bundle plan does not change
earlier bundle identities. Recovery can discover which prefix reached the vote
chain and let the user make a new decision about bundles that were never
submitted.

### 7. Select the framework through authenticated round configuration

Existing persisted authorities and authenticated round-auth version 2 rounds do
not contain a signed authority scheme and remain `random-v0`. Round-auth version
1 entries already fail current authentication and are skipped. A recoverable
round uses round-auth version 3 and carries these fields in its signed round
entry:

```text
auth_version = 3
snapshot_height = <unsigned 64-bit Zcash block height>
voting_authority_scheme = "recoverable-authority-v1"
bundle_policy = "recoverable-v1"
```

The canonical version 3 signing preimage is:

```text
round_auth_payload_v3 =
    "zcash-shielded-vote:round-auth:v3"
    || vote_round_id_32
    || ea_pk_32
    || pir_depth_u32_le
    || tier0_layers_u32_le
    || tier1_layers_u32_le
    || poly_len_u32_le
    || snapshot_height_u64_le
    || voting_authority_scheme_length_u16_le
    || voting_authority_scheme_ascii
    || bundle_policy_length_u16_le
    || bundle_policy_ascii
```

The version 2 round and PIR fields retain their encodings and relative order.
The snapshot height binds the note set from which the canonical bundle plan is
reconstructed. The two length-prefixed ASCII identifiers bind the authority
framework and note-selection policy. `vote-sdk` owns production of the exact
version 3 payload and signatures; `zcash_voting` owns byte-for-byte verification
and selection of the corresponding behavior.

The authenticated round returned by `zcash_voting` carries the signed snapshot
height. Construction and recovery obtain `VotingRoundParams::snapshot_height`
from that value. If vote-server metadata also supplies a height, it must match
the authenticated value; an implementation must not substitute the unsigned
copy.

The round configuration selects the common authority framework and bundle
policy. It does not select either local recovery source. A software wallet can
obtain its root through `registered-seed-v1`, while today's Keystone integration
uses `stored-random-v1`. A direct delegation obtains bundle material through
`derived-v1`; the typed public-target import uses
`imported-capability-v1`. Both root sources and both bundle sources are
indistinguishable on the vote chain.

After database loss, the wallet first restores the authenticated selection
marker. `zcash_voting` then constructs only the selected typed candidate from
explicit recovery inputs: the registered seed or authenticated root backup,
plus reconstructed local bundles or the validated custody capability. It
reconciles that candidate with the authority's durable external use.

A previously published target recovered from a validated capability or the
controller-owned durable job must match the selected root-derived hotkey and
`imported-capability-v1` flow; the exact capability is still required to restore
its bundles. A pending or confirmed direct delegation must match the selected
candidate's initial VANs in the transaction or validated vote tree. A mismatch
reports the missing or wrong recovery material instead of substituting another
source.

If the marker itself was lost, complete external evidence may reconstruct it
only when exactly one typed candidate matches: a public target identifies the
imported flow, while a pending or confirmed direct delegation identifies the
derived flow. The recovered selection is made durable again before use.
No match, multiple nonidentical matches, conflicting target and delegation
evidence, or incomplete target, transaction, or chain evidence fails closed.
Lack of evidence is not proof that no selection existed. These rules keep a
software-created authority recoverable from its seed without letting a software
wallet replace a Keystone-created authority merely because it can now access
the same account seed.

The selection marker is persisted outside `VotingDb` before direct delegation
or publication of a public target. The root is re-derived from the software seed
or held in the stored-random secure-storage and backup boundary. The typed
construction or import persists the bundle source with its bundle IDs or
capability digest. A wallet must not silently replace a missing root, re-derive
imported blindings, or reinterpret a capability bundle as `derived-v1`.

Failure is scoped to the object that could not be authenticated or interpreted.
An entry with an unknown auth version, authority scheme, or bundle policy is
excluded from the resolved round set and its round ID is reported, while other
authenticated rounds remain usable. An unknown persisted root source, bundle
source, or authority version rejects operations and recovery for that authority.
Authority fields on a version 1 or 2 entry are rejected because those versions
do not sign them. A wallet must not decide between legacy and version 1 behavior
from an application version, date, secret length, or whether local rows happen
to exist.

The vote chain does not need the scheme to verify proofs or signatures. A
future vote-action field may make scheme negotiation more visible, but it is
not required for the first implementation because round-auth version 3 binds
the value before the wallet constructs any delegation.

An older wallet that supports only version 2 does not authenticate or join a
version 3 round, but can continue to resolve other supported rounds. Activation
must not issue a version 2 attestation for the same new round as a compatibility
fallback; doing so would let an older wallet join under `random-v0` behavior.

The reverse transition is also forbidden. A round ID that has ever been
published with a version 1 or 2 attestation cannot later be published as version
3, even if the older entry was removed in between. Existing voters may already
have persisted or on-chain `random-v0` authority under that ID. Config
production checks the published round history before signing a version 3 entry,
and a wallet with an existing authority rejects an authenticated entry that
selects different authority semantics for the same round ID.

Version 3 intentionally preserves the version 2 preimage fields and adds the
snapshot height, but it does not add network or vote-chain ID. The trusted
signing-key set and signed config namespace must therefore be scoped to exactly
one Zcash network and one vote chain. If a deployment needs to reuse the same
trusted keys across either boundary, it needs a later auth version that signs
both identifiers.

## Software-wallet flow

### Initial delegation

1. Resolve and authenticate the round configuration.
2. Reconstruct the canonical snapshot note set and `recoverable-v1` bundle
   plan.
3. Ask the software seed provider for the version 1 authority root.
4. Select `derived-v1` and construct the hotkey, every bundle ID, and every
   blinding factor in `zcash_voting`.
5. Commit and read back the `registered-seed-v1` selection marker outside
   `VotingDb`. Persist bundle IDs, blindings, and plan metadata in `VotingDb`
   only for normal operation. Derive the root from the seed when needed, or
   cache it only in platform secure storage.
6. Build proofs, sign, submit, and confirm delegation through the existing
   lifecycle.

### Recovery

After a fresh install, the selection marker chooses the same seed provider and
bundle source, and the same authority context produces the same root. The
wallet can reconstruct the plan and recover on-chain VANs without a separate
voting-secret backup. If the marker was also lost, the complete external-use
reconciliation above must uniquely reconstruct it before recovery continues.

A watch-only wallet or imported UFVK cannot derive the root. Recovery requires
the original wallet seed or the exact authority root through an authenticated,
encrypted handoff.

## Current Keystone flow

Version 1 does not ask Keystone to derive, store, or export voting authority.
The host wallet uses `stored-random-v1` and the existing hardware interaction:

1. Resolve and authenticate the round configuration.
2. Extract the Orchard full viewing key from the paired account UFVK, compute
   the account fingerprint, and bind it into the authority context.
3. Generate the authority root and select `derived-v1`.
4. Complete the durable encrypted backup gate for that exact selection.
5. Let `zcash_voting` derive the hotkey, bundle IDs, VAN
   blindings, and delegation proof inputs.
6. Build the delegation PCZT or PCZT batch.
7. Ask Keystone to sign it through the existing Zcash signing flow.
8. Submit and confirm delegation through the existing lifecycle.

Multiple delegation PCZTs can continue to use the existing Keystone batch
signing path. A batch contains already-constructed transactions, so grouping
them for one signing interaction does not alter any authority derivation.

After delegation, the host-held voting hotkey signs vote-chain actions as it
does today. No additional Keystone operation is introduced.

### Keystone recovery

Restoring the Keystone mnemonic or pairing the device restores access to the
Zcash account but not the random voting-authority root. The wallet must also
restore the encrypted `VotingAuthorityBackupV1` payload and obtain the
account's Orchard viewing capability or equivalent snapshot-note state. The
viewing data may come from the normal encrypted wallet backup; otherwise the
host can obtain the UFVK by pairing the restored Keystone account.

The wallet first validates the backup's account fingerprint, then
`zcash_voting` recreates the hotkey, canonical `derived-v1` bundle plan, and
every bundle blinding. Keystone is needed to sign only when a remaining
delegation transaction still requires the Zcash account signature. Once the
viewing data and authority backup are present, a voter whose delegation is
already confirmed can use the host-side voting authority without a Keystone
signature or new device derivation operation.

If the exact root backup is unavailable, recovery fails closed. The wallet must
not generate a replacement root and present it as the original authority.

## Future Keystone provider seam

A future firmware project may implement an authority-root provider for the same
canonical context and 64-byte result. That feature could match
`registered-seed-v1` or define a separately versioned provider source. Its user
approval, transport confidentiality, account authentication, and firmware
resource limits would receive a separate design review.

Nothing after the provider boundary changes:

- `VotingAuthorityContextV1` remains the request context;
- `VotingAuthorityRootV1` remains the returned secret type;
- hotkey expansion remains byte-for-byte identical;
- local `VotingBundleIdV1` and `van_comm_rand` derivation remain identical;
- imported custody capabilities remain unchanged;
- recovery continues to locate canonical VANs; and
- circuits and vote-chain messages remain unchanged.

A future deterministic provider applies to newly created authorities. It
cannot retroactively derive a previously sampled `stored-random-v1` root from
the Keystone mnemonic. Existing random-root rounds continue to recover from
their backups.

This seam also allows a future device to import or retain an exact root if that
device-storage model is separately approved. Version 1 neither requires nor
specifies that behavior.

## Batching compatibility

Authority construction happens before transaction batching and is keyed to a
delegation bundle, not to a transaction container.

The current atomic vote API carries multiple ordered proposal actions for one
delegation `bundle_index`. The actions share a hotkey, start from that bundle's
VAN, and use the existing ordered transition and batch-digest rules. Batch size,
proposal order, digest, transaction hash, and proof or signature randomness do
not alter the root, hotkey, bundle identity, or selected initial VAN blinding.

If a future vote-chain transaction can carry actions from several delegation
bundle indices, each bundle still has its own derived or imported blinding and
independent VAN transition chain. The transaction layer may commit those chains
atomically, but it does not merge their identities or create a batch-wide
`van_comm_rand`. No version 1 authority change would be required.

## Recovery state machine

`zcash_voting` recovers one bundle as follows:

1. Build the typed source candidates above, reconcile them with durable external
   use, and validate the selected root source, bundle source, authority context,
   account fingerprint, scheme, and bundle policy.
2. Re-derive the hotkey from the authority root.
3. Restore the bundle material. For `derived-v1`, rescan the account at the
   round snapshot, rebuild the canonical bundle plan including notes spent
   since the snapshot, and re-derive each bundle ID, blinding, and weight. For
   `imported-capability-v1`, restore the exact capability package, validate its
   target and round against the recovered hotkey, and import its bundle indices,
   weights, blindings, and transaction hashes.
4. Reconstruct each initial VAN from the recovered hotkey and bundle material.
5. Sync and root-validate the vote-commitment tree and the complete confirmed
   cast-vote transition index through the same authenticated chain height.
6. Locate the unique initial VAN in the validated tree and record its leaf
   position.
7. Starting from that VAN and its initial mask, derive its spend nullifier and
   look it up in the transition index.
8. If no confirmed transition consumes the nullifier, the current VAN and
   position have been found. Otherwise validate the singleton or ordered atomic
   batch, derive its successor mask and VAN, require the final commitment at the
   reported leaf position in the validated tree, and repeat.

The transition index is built from every confirmed `cast_vote` and
`cast_vote_batch` action for the round. It may come from a complete verified
block scan or an index that proves both inclusion and absence against
authenticated chain state. A best-effort transaction search is not sufficient:
absence is used only when completeness through the selected height is known.
Each record exposes the consumed VAN nullifier, ordered proposal IDs, action
nullifiers and successor commitments, each vote commitment and its confirmed
vote-commitment tree position, and the final VAN leaf position.

Validation recomputes every native authority transition from the recovered
hotkey, bundle material, and current mask. It requires the first nullifier to
consume the current VAN, every proposal bit to be available and used once, all
ordered action nullifiers and successor commitments to match, and the final VAN
to occupy the reported tree leaf. A duplicate consumer, malformed batch,
unexpected proposal transition, missing final leaf, or index/tree height
mismatch fails closed. An atomic batch contributes several transitions but only
one final tree leaf.

The initial mask is `0xFFFF`. Bit 0 remains set, while bits 1 through 15 identify
unused proposals. Recovery therefore performs at most 15 native transitions and
index lookups per bundle instead of enumerating `2^15` masks. At the existing
4,096-bundle capability limit, the aggregate bound is 61,440 transitions. The
index can be loaded once and shared across all bundles.

The final record provides the current VAN position and proposal-authority mask.
For a bundle known to have a confirmed delegation, a missing initial VAN,
ambiguous transition, or validation failure stops recovery. A bundle with no
initial VAN may continue direct delegation only after ordinary transaction
reconciliation establishes that it was never confirmed and its source notes
remain usable. An imported custody bundle remains under the controller's
existing signed-transaction reconciliation and redelivery flow; the voter does
not rebuild the controller's delegation.

This requires complete public round transition data, not the wallet's prior
transaction database or ballot choices. A stale vote built from an earlier VAN
is rejected by the chain as a spent nullifier; recovery finds the current VAN
instead of using submission as a probe. A stale or withholding chain source
stops recovery rather than making absence evidence that a VAN is unused. It
still cannot recreate a missing custody capability.

### Pending tally recovery

Finding the current VAN does not recover a confirmed vote whose helper shares
are still incomplete. Version 1 therefore adds a library-owned
`PendingTallyRecoveryV1` record for the interval between committing a vote and
confirming all of its expected helper shares. It contains:

- the exact serialized `VoteRecoveryBundle` for each action, including its
  secret share material and, for an atomic batch, the common digest and complete
  action order;
- each real confirmed vote-commitment tree position once known;
- the helper-delivery journal for each share, including its identity, original
  `created_at`, `submit_at`, target count, accepted (`sent_to_urls`),
  `ambiguous_urls`, and `attempting_urls` helper sets, and confirmation state;
  and
- a stable record ID, monotonic revision, and prior-revision digest.

`zcash_voting` owns the versioned record, validation, and conservative merge
rules. The integration stores it in authenticated encrypted storage that
survives loss of `VotingDb`; it is not sent to the vote chain or helpers. The
record is written and read back before the first vote broadcast. Confirmation
adds the authenticated tree position before helper delivery starts. If the
database was lost first, the position may instead be recovered from the exact
matching action in validated chain data. A placeholder position is never used.
An action not found there remains subject to ordinary transaction
reconciliation; its recovered message or complete batch can be resubmitted
without preserving the original outer transaction bytes.

The backup slot must atomically retain and return its latest committed revision;
an older but otherwise valid snapshot is not sufficient. Updates use
compare-and-swap or an append-only equivalent, and restore rejects a broken
revision chain or two different records at the same revision.

Before each helper POST, the corresponding attempting state is made durable.
The accepted, ambiguous, definite-failure, or confirmed transition is then
recorded before another helper is contacted. Restore keeps accepted,
ambiguous, and attempting evidence, never reduces the target count or changes
the original schedule, and resumes the existing helper tracker against the
current validated helper configuration. Conflicting identity, schedule, batch,
or tree-position data fails closed. These are the same ordering and retry rules
used by the live tracker, not a second delivery policy.

All actions in an atomic batch are committed to this backup together; a partial
batch record is invalid. Helper tracking remains per action and share. Once all
expected shares are confirmed on the vote chain, the record is deleted. The
wallet does not retain complete vote history: after deletion, VAN transition
recovery can still show that a proposal was already consumed, but it does not
recover the earlier ballot choice.

## Legacy migration

Existing rows and rounds remain `random-v0`:

- their 64-byte stored hotkey secret keeps its current ZIP-32-seed
  interpretation;
- their persisted `van_comm_rand` values remain authoritative; and
- no code attempts to replace them with version 1 derivations.

An on-chain VAN cannot be migrated to a new hotkey or blinding factor. If the
legacy secrets are already gone, version 1 cannot recover them retroactively.

As a short-term mitigation, a wallet that still has a live legacy round may
offer an encrypted full-round backup containing the hotkey secret and every
bundle blinding factor. That legacy package is distinct from
`VotingAuthorityBackupV1`.

Persisted state adds explicit authority scheme, root source, bundle source,
authority context, and either bundle-ID or capability-digest metadata. It does
not add the version 1 root or a cached Orchard spending key to `VotingDb`.
Migration assigns `random-v0` to all existing rows. It never infers a scheme
from secret length or recomputes a value merely because a column is empty.

Version 1 does not supersede a `stored-random-v1` backup. Its root, root source,
bundle source, and context remain fixed from creation, including before a public
target is published or a direct delegation is submitted. A later design that
supports authority rotation must give restore an authenticated, durable way to
distinguish generations. After publication or possible submission, every
authority source remains immutable.

## Public-target custody

The existing public-target flow remains part of
`recoverable-authority-v1`. The voter obtains an authority root from either
version 1 root source, derives the hotkey, and sends the existing round-bound
public target. The authority context is bound to the voter's selected account;
it does not claim that the voter owns the funds controller's account. The
selected voter account must be Orchard-capable but need not contain the funds
being delegated by the controller.

The funds controller applies `recoverable-v1` to its own snapshot notes,
samples each VAN blinding as it does today, constructs ZKP #1, and exports the
unchanged `DelegationCapabilityV1`. The controller never receives the authority
root or hotkey secret. The voter does not send bundle IDs or blindings to the
controller.

On import, `zcash_voting` verifies that the capability's target, network, chain,
and round match the root-derived hotkey and authenticated configuration. It
records `imported-capability-v1` and uses the package's bundle indices, weights,
blindings, and transaction hashes through the same VAN and voting APIs used by
direct delegation.

The root and capability have separate recovery jobs:

- the root recreates the voting hotkey; and
- the exact capability recreates the custody bundle material.

The voter must retain the imported package durably, while the funds controller
keeps the existing redeliverable outbox copy. Either copy can restore the
bundle material. If both copies are lost, the root still recovers the hotkey but
cannot recover those bundle blindings. No circuit, vote-chain message,
capability format, or additional custody exchange changes in version 1.

[zip32-registered]: https://zips.z.cash/zip-0032#specification-registered-key-derivation

## Repository responsibilities

### `zcash_voting`

- account fingerprint, authority context, source-selection, and root-backup
  types;
- hotkey, local bundle-ID, and VAN-blinding derivation;
- canonical validation and recovery of imported custody capabilities;
- fail-closed reconciliation of typed source candidates with external authority
  use;
- the pending-tally record and helper-journal restore rules;
- the recoverable bundle policy and round-auth version 3 verification;
- legacy migration and bounded nullifier-to-successor VAN recovery; and
- atomic-batch rules, public vectors, and integration fixtures.

### All wallet integrations

- durably store and read back the encrypted pending-tally record before vote or
  helper network effects;
- restore it outside `VotingDb` and pass it back through the canonical
  `zcash_voting` importer; and
- delete it only after all expected helper shares are confirmed.

### Software-wallet integrations

- root-seed provider implementation;
- durable authenticated storage of the software selection marker;
- secure root handling and backup where applicable;
- snapshot rescan, external-use reconciliation, and recovery UX; and
- fail-closed handling for unsupported schemes or custom non-recoverable note
  selection.

### Public-target integrations

- creating the public target from a version 1 root-derived hotkey;
- returning the previously published target during recovery;
- preserving the existing controller-side blinding generation, ZKP #1, and
  capability export;
- durable voter retention and controller redelivery of the exact capability;
  and
- selecting `imported-capability-v1` only through the typed import API.

### Current Keystone integrations

- calling the canonical stored-random root generator;
- deriving and validating the account fingerprint from the paired UFVK;
- secure live storage, encrypted root backup, and the pre-delegation backup
  gate;
- restored account-viewing-state UX; and
- existing Keystone PCZT or PCZT-batch signing.

### Keystone firmware

No version 1 change is required.

A future firmware project may implement a root provider against the canonical
context, root type, and expansion vectors. That work is not an activation gate
for this design.

### `voting-circuits`

No circuit change is required. Builder documentation must permit either a
CSPRNG-sampled custody blinding or a domain-separated PRF output uniformly
mapped into the Pallas base field. Cross-repository fixtures should prove that
both sources produce the expected public VAN and valid ZKP #1 and ZKP #2
statements.

### `vote-sdk`

No consensus rule or vote-action message change is required for authority
construction. `vote-sdk` must add the round-auth version 3 config fields,
including the snapshot height, canonical signing preimage, signer and config-PR
support, and verification fixtures shared with `zcash_voting`. An explicit
on-chain scheme field may be considered later, but is not needed when
round-auth version 3 is enforced.
