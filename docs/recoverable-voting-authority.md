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
always recreates the voting hotkey. For locally controlled funds,
`zcash_voting` also derives each bundle's VAN blinding from the root and
canonical bundle plan; public-target custody instead restores bundle blindings
and weights from the existing capability package retained by the voter and
funds controller. The signed round configuration selects this common version 1
framework. After data loss, the restored root and bundle material let the
wallet find its latest VANs in the validated vote tree and continue after
partial delegation or singleton and atomic votes. Existing rounds remain
unchanged.

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
| Software seed provider | Original wallet seed or mnemonic plus a rescan of the original account | Re-derive the same authority root and canonical bundle plan |
| Current Keystone integration with a confirmed delegation | Encrypted voting-authority backup plus the original account's Orchard viewing capability or equivalent restored snapshot-note state | Restore the root and canonical bundle plan, then vote without a Keystone signature or device operation |
| Current Keystone integration with remaining delegation transactions | Encrypted voting-authority backup plus access to the original Zcash account, normally through Keystone | Restore the root, then sign the remaining delegation PCZTs |
| Public-target custody with retained capability | Voter's seed or encrypted authority-root backup plus the exact custody capability package | Re-derive the hotkey, import the original bundle weights and blindings, then recover matching VANs |
| Public-target custody without either retained or redeliverable capability | No bundle recovery | The root recreates the hotkey but not custody-provider blindings |
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
  without requiring transaction history.
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
- Recover random secrets that were already lost in a legacy round.
- Change the ZKP statements, on-chain messages, or vote-chain verifier.
- Move ZKP generation or vote signing into Keystone.
- Make an imported UFVK sufficient to reconstruct private voting authority.
- Recover an earlier ballot choice, helper-delivery state, or exact unconfirmed
  transaction after the local voting database is lost.
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

`network_tag_u8` is the single byte `0` for mainnet, `1` for testnet, and `2`
for regtest. `account_fingerprint_32` must match the account selected by
`account_index_u32_le`; a provider rejects a mismatch before returning or
restoring a root.
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
publication of a public target and are persisted explicitly. They must not be
inferred from application version, secret length, device type, or the presence
of local rows. Once any delegation may have been submitted, both sources and
the root are immutable for that authority.

The root source is intentionally not included in the expansion. If two
conforming providers return the same root for the same context, they produce
the same hotkey and, for `derived-v1`, the same bundle blindings. This is what
allows a future Keystone provider to join without another downstream
derivation scheme.

### 2. Derive software-wallet roots from the wallet seed

A software wallet uses the [ZIP 32 registered-key construction][zip32-registered]
under the application-owned context `ZcashShieldedVoting` and a fixed numeric
namespace constant, `VOTING_KDF_ID_V1`, defined by `zcash_voting`.

Neither the context nor the numeric namespace requires external assignment or
a standalone ZIP before deployment. `zcash_voting` defines both as version 1
protocol constants. Changing either produces different roots and therefore a
new root-source version. A later ZIP may document the deployed values without
renumbering version 1.

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

The registered cryptovalue is not passed into any ZIP-32 KDF. The next section
defines a direct Orchard-key expansion instead.

### 3. Back up randomly generated roots before use

Today's Keystone firmware cannot supply the registered seed-derived root. A
Keystone integration therefore uses `stored-random-v1`:

1. Ask `zcash_voting` to generate 64 bytes with the operating system CSPRNG.
2. Bind the root to the canonical authority context in a typed backup record.
3. Store the live copy in platform secure storage.
4. Commit an authenticated, encrypted backup that can survive deletion or loss
   of the app's voting database.
5. Read back and validate the backup before publishing a public target or
   allowing any direct delegation submission.

The canonical plaintext payload is `VotingAuthorityBackupV1`. It contains the
format version, `stored-random-v1` root-source identifier, selected bundle
source, complete authority context, and 64-byte root. `zcash_voting` owns this
payload and its validation; the wallet integration owns encryption,
authentication, storage, and restore UX. The plaintext payload must never be
logged, placed in a transaction memo, or included in diagnostics.

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

The hotkey uses external Orchard address index zero. Its persisted authority is
a typed root, not an untagged byte string whose meaning is guessed from length:

```text
VotingAuthoritySecret::RandomV0(64-byte legacy hotkey seed)
VotingAuthoritySecret::RootV1 {
    root_source,
    bundle_source,
    context,
    authority_root_64,
}
```

Implementations may cache the derived 32-byte Orchard spending key, but the
root remains the recovery source. Both root and derived key are zeroized after
use.

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

`recoverable-v1` selects the account's canonical eligible note set at the
round snapshot. Before planning, candidate notes are ordered by commitment-tree
position with note commitment as a tiebreaker, then duplicate nullifiers are
collapsed by retaining the first note in that order. It applies these frozen
planning values:

| Property | `recoverable-v1` value |
| --- | --- |
| Real notes per bundle | `BUNDLE_NOTE_SLOTS` (currently 5) |
| Bundle-addition value threshold | Disabled |
| Zero-ballot bundle rule | Drop totals below `BALLOT_DIVISOR` (12,500,000 zatoshi) |
| Packing order | Value descending, then position ascending |
| Notes within a surviving bundle | Position ascending |
| Surviving bundle order | Total value descending, then minimum position ascending |
| Privacy bundle target | 2 |
| Privacy drop budget | 1% of selected note value |
| Absolute privacy drop ceiling | 100,000,000,000 zatoshi (1,000 ZEC) |

The privacy trim greedily removes the lowest-value trailing bundle while both
the bundle target and drop budget permit, preserving at least one eligible
bundle. These rules intentionally match the current default policy except for
making the initial note order and duplicate choice explicit. Later changes
require a new policy identifier. They do not silently change version 1.

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

Rounds created before activation use round-auth version 1 or 2, do not contain
an authenticated scheme, and remain `random-v0`. A recoverable round uses
round-auth version 3 and carries these fields in its signed round entry:

```text
auth_version = 3
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
    || voting_authority_scheme_length_u16_le
    || voting_authority_scheme_ascii
    || bundle_policy_length_u16_le
    || bundle_policy_ascii
```

The first seven fields preserve the version 2 round and PIR bindings. The two
length-prefixed ASCII identifiers additionally bind the authority framework
and note-selection policy. `vote-sdk` owns production of the exact version 3
payload and signatures; `zcash_voting` owns byte-for-byte verification and
selection of the corresponding behavior.

The round configuration selects the common authority framework and bundle
policy. It does not select either local recovery source. A software wallet can
obtain its root through `registered-seed-v1`, while today's Keystone integration
uses `stored-random-v1`. A direct delegation obtains bundle material through
`derived-v1`; the typed public-target import uses
`imported-capability-v1`. Both root sources and both bundle sources are
indistinguishable on the vote chain.

The root source and root are persisted before direct delegation or publication
of a public target. The typed construction or import persists the bundle source
with its bundle IDs or capability digest. A wallet must not silently replace a
missing root, re-derive imported blindings, or reinterpret a capability bundle
as `derived-v1`.

The crate fails closed on an unknown auth version, scheme, root source, bundle
source, or policy. It rejects authority fields attached to a version 1 or 2
entry because those versions do not sign the fields. A wallet must not decide
between legacy and version 1 behavior from an application version, date, secret
length, or whether local rows happen to exist.

The vote chain does not need the scheme to verify proofs or signatures. A
future vote-action field may make scheme negotiation more visible, but it is
not required for the first implementation because round-auth version 3 binds
the value before the wallet constructs any delegation.

An older wallet that supports only auth versions 1 and 2 rejects version 3
rather than ignoring an unsigned extension. Activation must not issue a
version 1 or 2 attestation for the same new round as a compatibility fallback;
doing so would let an older wallet join under `random-v0` behavior.

## Software-wallet flow

### Initial delegation

1. Resolve and authenticate the round configuration.
2. Reconstruct the canonical snapshot note set and `recoverable-v1` bundle
   plan.
3. Ask the software seed provider for the version 1 authority root.
4. Select `derived-v1` and construct the hotkey, every bundle ID, and every
   blinding factor in `zcash_voting`.
5. Persist the root source, `derived-v1` bundle source, context, root, bundle
   IDs, blindings, and plan for normal operation. The seed remains the
   independent recovery source.
6. Build proofs, sign, submit, and confirm delegation through the existing
   lifecycle.

### Recovery

After a fresh install, the same seed provider and authority context produce the
same root. The wallet can reconstruct the plan and recover on-chain VANs without
a separate voting-secret backup.

A watch-only wallet or imported UFVK cannot derive the root. Recovery requires
the original wallet seed or the exact authority root through an authenticated,
encrypted handoff.

## Current Keystone flow

Version 1 does not ask Keystone to derive, store, or export voting authority.
The host wallet uses `stored-random-v1` and the existing hardware interaction:

1. Resolve and authenticate the round configuration.
2. Extract the Orchard full viewing key from the paired account UFVK, compute
   the account fingerprint, and bind it into the authority context.
3. Generate the authority root and complete the durable encrypted backup gate.
4. Select `derived-v1` and let `zcash_voting` derive the hotkey, bundle IDs, VAN
   blindings, and delegation proof inputs.
5. Build the delegation PCZT or PCZT batch.
6. Ask Keystone to sign it through the existing Zcash signing flow.
7. Submit and confirm delegation through the existing lifecycle.

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

1. Validate the root source, bundle source, authority context, account
   fingerprint, scheme, and bundle policy.
2. Re-derive the hotkey from the authority root.
3. Restore the bundle material. For `derived-v1`, rescan the account at the
   round snapshot, rebuild the canonical bundle plan including notes spent
   since the snapshot, and re-derive each bundle ID, blinding, and weight. For
   `imported-capability-v1`, restore the exact capability package, validate its
   target and round against the recovered hotkey, and import its bundle indices,
   weights, blindings, and transaction hashes.
4. Reconstruct each initial VAN from the recovered hotkey and bundle material.
5. Sync and root-validate the vote-commitment tree.
6. Derive the VAN for every mask obtainable by clearing a subset of the
   round's proposal bits and locate matching commitments in the tree.
7. Require each later tree match's set bits to be a strict subset of the
   previous match's set bits, then select the unique match with the most
   consumed proposal bits.

The current circuit has 15 usable proposal bits, so this search has at most
`2^15` candidates per bundle. The initial mask is `0xFFFF`. Bit 0 remains set;
bits 1 through 15 identify unused proposals. A singleton appends each successor
VAN. An atomic batch appends only its final VAN, but that commitment still
contains the final mask, which is all recovery needs to continue voting.

The selected match provides the current VAN position and proposal-authority
mask. For a bundle known to have a confirmed delegation, no match, incomparable
matches, multiple terminal matches, or any tree mismatch fail closed. A bundle
with no matches may continue direct delegation only after ordinary transaction
reconciliation establishes that it was never confirmed and its source notes
remain usable. An imported custody bundle remains under the controller's
existing signed-transaction reconciliation and redelivery flow; the voter does
not rebuild the controller's delegation.

Transaction history can improve status and diagnostics, but it is not a
recovery requirement. A stale vote built from an earlier VAN is rejected by the
chain as a spent nullifier; recovery should find the current VAN instead of
using submission as a probe. If a future circuit expands the authority mask
enough to make enumeration impractical, a targeted nullifier-to-successor query
can replace the search without changing authority derivation.

Version 1 does not reconstruct an earlier choice, helper-delivery state, atomic
batch order, or exact unconfirmed transaction after the voting database is
lost. It also cannot recreate a missing custody capability. Those are separate
transaction and backup concerns.

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
authority context, and either bundle-ID or capability-digest metadata. Migration
assigns `random-v0` to all existing rows. It never infers a scheme from secret
length or recomputes a value merely because a column is empty.

Changing the root, provider, or bundle source is allowed only before any
delegation may have been submitted. After that point, the original authority
remains mandatory.

## Public-target custody

The existing public-target flow remains part of
`recoverable-authority-v1`. The voter obtains an authority root from either
version 1 root source, derives the hotkey, and sends the existing round-bound
public target. The authority context is bound to the voter's selected account;
it does not claim that the voter owns the funds controller's account.

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

- account fingerprint, authority context, root source, bundle source, and backup
  types;
- hotkey, local bundle-ID, and VAN-blinding derivation;
- canonical validation and recovery of imported custody capabilities;
- the recoverable bundle policy and round-auth version 3 verification;
- legacy migration and current-VAN tree recovery; and
- atomic-batch rules, public vectors, and integration fixtures.

### Software-wallet integrations

- root-seed provider implementation;
- secure root handling and backup where applicable;
- snapshot rescan and recovery UX; and
- fail-closed handling for unsupported schemes or custom non-recoverable note
  selection.

### Public-target integrations

- creating the public target from a version 1 root-derived hotkey;
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
canonical signing preimage, signer and config-PR support, and verification
fixtures shared with `zcash_voting`. An explicit on-chain scheme field may be
considered later, but is not needed when round-auth version 3 is enforced.
