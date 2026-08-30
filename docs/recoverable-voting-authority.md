# Recoverable voting authority design

## Status

This document is a proposal for review. It does not describe behavior that is
implemented today.

The review goal is to approve the authority model, derivation boundaries,
backup contract, recovery behavior, batching compatibility, and migration
boundary before code is written.

Deterministic Keystone hotkey derivation is explicitly outside this version.
The version 1 Keystone path uses the firmware and transaction-signing flow that
exist today.

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

Version 1 introduces one 64-byte **voting authority root** for each Zcash
account, network, vote chain, and round. `zcash_voting` expands that root into:

- one Orchard voting hotkey; and
- one deterministic, independent VAN blinding factor for each canonical
  delegation bundle.

The root can come from more than one source without changing the expansion:

- a software wallet derives it from its wallet seed; or
- an integration that cannot derive it from its seed, including today's
  Keystone integrations, generates it randomly and makes a durable encrypted
  backup before delegation can be submitted.

```text
Software wallet seed ---- software seed provider ---+
                                                     |
Stored random root ------- encrypted backup --------+--> authority root v1
                                                               |
                                                               v
                                                         zcash_voting
                                                      /                \
                                            voting hotkey        bundle VAN blinds
```

For a current Keystone wallet, the hotkey is therefore not recoverable from the
Keystone mnemonic. It is recoverable from the separate voting-authority backup.
Keystone continues to sign the completed delegation PCZT or PCZT batch exactly
as it does today; no new firmware operation or QR type is required.

The root-provider boundary is intentional. A future Keystone firmware feature
can derive and return the same kind of root. The hotkey expansion, bundle
identity, VAN blinding derivation, recovery state machine, circuits, and
vote-chain messages would not need to change.

The authority derivation is client-side. The voting circuits and vote-chain
verifier do not need to know whether a private witness was sampled or derived.
After restoration, the same root recreates the hotkey and bundle blindings so
the wallet can find its matching VANs in the validated vote tree and continue
voting.

### Recovery at a glance

| Authority source | Recovery material | Result |
| --- | --- | --- |
| Software seed provider | Original wallet seed or mnemonic plus a rescan of the original account | Re-derive the same authority root and canonical bundle plan |
| Current Keystone integration with a confirmed delegation | Encrypted voting-authority backup plus the original account's Orchard viewing capability or equivalent restored snapshot-note state | Restore the root and canonical bundle plan, then vote without a Keystone signature or device operation |
| Current Keystone integration with remaining delegation transactions | Encrypted voting-authority backup plus access to the original Zcash account, normally through Keystone | Restore the root, then sign the remaining delegation PCZTs |
| Keystone wallet or mnemonic without the voting-authority backup | No recovery | The Keystone seed does not contain the randomly generated root |
| UFVK or watch-only wallet | No recovery by itself | Public keys cannot reveal voting authority |
| Legacy `random-v0` round | Original hotkey and every original bundle blinding | Version 1 cannot recreate already-lost random values |

The viewing capability in the confirmed-Keystone row may already be present in
the wallet's normal encrypted backup. In that case the Keystone device is not
needed. The authority backup by itself can re-derive the hotkey, but it cannot
reconstruct bundle IDs or weights without the original account's snapshot
notes.

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

## Goals

- Restore the same software-wallet voting hotkey from the same wallet seed,
  account, network, vote chain, and round.
- Restore the same current-Keystone voting hotkey from a durable encrypted
  voting-authority backup, without changing Keystone firmware.
- Restore a different, deterministic VAN blinding factor for every canonical
  delegation bundle.
- Give every integration one canonical `zcash_voting` API after the authority
  root has been supplied.
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
- Replace the existing public-target custody handoff in the first version.
  A future version can add a two-party exchange for deterministic bundle
  blindings without changing the self-custody design in this document.
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

The canonical account and round identity used by every version 1 expansion:

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
logic. It recreates the hotkey and bundle blindings but cannot spend the Zcash
account.

### Authority root source

The local method used to obtain a root. Version 1 defines
`registered-seed-v1` and `stored-random-v1`. The source is recovery metadata; it
does not alter root expansion or appear on-chain.

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

### 1. Standardize one provider-independent root boundary

`zcash_voting` owns these concepts:

```text
VotingAuthorityContextV1
VotingAuthorityRootV1(64 bytes)
AuthorityRootSourceV1
```

A root provider accepts the canonical context and returns exactly one 64-byte
root. After that call, all providers use the same `zcash_voting` expansion.
Integrations must not independently define hotkey, bundle-ID, or VAN-blinding
derivation.

Before returning a root, the provider validates that the context's account
fingerprint matches the Orchard full viewing key for the selected network and
account index. This prevents a backup or randomly generated root belonging to
another seed's account at the same index from being accepted as the current
authority.

The provider source is selected before the first delegation is built and is
persisted explicitly. It must not be inferred from application version, secret
length, device type, or the presence of local rows. Once any delegation may
have been submitted, the provider and root are immutable for that authority.

The root source is intentionally not included in the expansion. If two
conforming providers return the same root for the same context, they produce
the same voting authority. This is what allows a future Keystone provider to
join without another downstream derivation scheme.

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

### 3. Back up randomly generated roots before Keystone delegation

Today's Keystone firmware cannot supply the registered seed-derived root. A
Keystone integration therefore uses `stored-random-v1`:

1. Ask `zcash_voting` to generate 64 bytes with the operating system CSPRNG.
2. Bind the root to the canonical authority context in a typed backup record.
3. Store the live copy in platform secure storage.
4. Commit an authenticated, encrypted backup that can survive deletion or loss
   of the app's voting database.
5. Read back and validate the backup before allowing any delegation submission.

The canonical plaintext payload is `VotingAuthorityBackupV1`. It contains the
format version, `stored-random-v1` source identifier, complete authority
context, and 64-byte root. `zcash_voting` owns this payload and its validation;
the wallet integration owns encryption, authentication, storage, and restore
UX. The plaintext payload must never be logged, placed in a transaction memo,
or included in diagnostics.

Because the complete context includes `account_fingerprint_32`, restore must
compare the record with the currently selected Orchard full viewing key before
making the root available. Matching network, account index, chain, and round is
not sufficient by itself.

Acceptable storage can include an integration's existing end-to-end encrypted
wallet backup or an explicit encrypted export. A second row in the same voting
database is not a backup. A device-only Keychain entry is useful live storage
but is not sufficient by itself if it disappears with the device or app.

If the integration cannot complete and verify the durable backup, it must not
label the authority recoverable or submit a delegation under
`authority-root-v1`. This is a real safety gate: after a delegation reaches the
vote chain, no different root can take over its authority.

The backup replaces the need to preserve every bundle blinding separately. It
does not contain the Zcash account's snapshot notes or viewing capability. A
confirmed delegation can be exercised without a new Keystone signature once
both that account data and the voting-authority root are restored. Remaining
delegation transactions additionally need access to the Zcash account signer.

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
    source,
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

### 5. Bind each VAN blinding factor to a canonical bundle identity

A round can have multiple delegation bundles that share one hotkey. Each bundle
must receive independent blinding material. Bundle index alone is not enough:
an SDK policy change could assign different notes to the same index after a
database loss.

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

### 6. Freeze the recoverable note-selection policy

Deterministic secrets are necessary but not sufficient if a restored wallet
cannot reconstruct the original bundle weights. Version 1 recovery therefore
uses a named, frozen snapshot-note selection and `BundlePolicy` definition
owned by `zcash_voting`.

The authenticated round configuration identifies:

- `voting_authority_scheme = "authority-root-v1"`; and
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

An intentionally skipped suffix of the canonical bundle plan does not change
earlier bundle identities. Recovery can discover which prefix reached the vote
chain and let the user make a new decision about bundles that were never
submitted.

### 7. Select the expansion through authenticated round configuration

Rounds created before activation use round-auth version 1 or 2, do not contain
an authenticated scheme, and remain `random-v0`. A recoverable round uses
round-auth version 3 and carries these fields in its signed round entry:

```text
auth_version = 3
voting_authority_scheme = "authority-root-v1"
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
length-prefixed ASCII identifiers additionally bind the authority expansion
and note-selection policy. `vote-sdk` owns production of the exact version 3
payload and signatures; `zcash_voting` owns byte-for-byte verification and
selection of the corresponding behavior.

The round configuration selects the canonical expansion and bundle policy. It
does not select the local root source. A software wallet can use
`registered-seed-v1`, while today's Keystone integration uses
`stored-random-v1`; both result in an indistinguishable version 1 authority on
the vote chain.

The chosen source and root are persisted together before delegation. A wallet
must not silently fall back to a new random root when seed derivation or backup
restore fails.

The crate fails closed on an unknown auth version, scheme, source, or policy.
It rejects authority fields attached to a version 1 or 2 entry because those
versions do not sign the fields. A wallet must not decide between legacy and
version 1 behavior from an application version, date, secret length, or whether
local rows happen to exist.

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
4. Construct the hotkey, every bundle ID, and every blinding factor in
   `zcash_voting`.
5. Persist the source, context, root, bundle IDs, blindings, and plan for normal
   operation. The seed remains the independent recovery source.
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
4. Let `zcash_voting` derive the hotkey, bundle IDs, VAN blindings, and
   delegation proof inputs.
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
`zcash_voting` recreates the hotkey, canonical bundle plan, and every bundle
blinding. Keystone is needed to sign only when a remaining delegation
transaction still requires the Zcash account signature. Once the viewing data
and authority backup are present, a voter whose delegation is already
confirmed can use the host-side voting authority without a Keystone signature
or new device derivation operation.

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
- `VotingBundleIdV1` and `van_comm_rand` derivation remain identical;
- recovery continues to locate canonical VANs; and
- circuits and vote-chain messages remain unchanged.

A future deterministic provider applies to newly created authorities. It
cannot retroactively derive a previously sampled `stored-random-v1` root from
the Keystone mnemonic. Existing random-root rounds continue to recover from
their backups.

This seam also allows a future device to import or retain an exact root if that
custody model is separately approved. Version 1 neither requires nor specifies
that behavior.

## Batching compatibility

Authority derivation happens before transaction batching and is keyed to a
canonical delegation bundle, not to a transaction container.

The current atomic vote API carries multiple ordered proposal actions for one
delegation `bundle_index`. The actions share a hotkey, start from that bundle's
VAN, and use the existing ordered transition and batch-digest rules. Batch size,
proposal order, digest, transaction hash, and proof or signature randomness do
not enter the root, hotkey, bundle ID, or initial VAN-blinding derivation.

If a future vote-chain transaction can carry actions from several delegation
bundle indices, each bundle still has its own root-derived blinding and
independent VAN transition chain. The transaction layer may commit those chains
atomically, but it does not merge their identities or derive a batch-wide
`van_comm_rand`. No version 1 derivation change would be required.

## Recovery state machine

`zcash_voting` recovers one bundle as follows:

1. Validate the root source, authority context, account fingerprint, scheme,
   and bundle policy.
2. Rescan the account at the round snapshot and rebuild the canonical bundle
   plan, including notes spent since the snapshot.
3. Re-derive the hotkey, bundle ID, blinding, weight, and initial VAN.
4. Sync and root-validate the vote-commitment tree.
5. Derive the VAN for every mask obtainable by clearing a subset of the
   round's proposal bits and locate matching commitments in the tree.
6. Require each later tree match's set bits to be a strict subset of the
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
with no matches may continue normal delegation only after ordinary transaction
reconciliation establishes that it was never confirmed and its source notes
remain usable.

Transaction history can improve status and diagnostics, but it is not a
recovery requirement. A stale vote built from an earlier VAN is rejected by the
chain as a spent nullifier; recovery should find the current VAN instead of
using submission as a probe. If a future circuit expands the authority mask
enough to make enumeration impractical, a targeted nullifier-to-successor query
can replace the search without changing authority derivation.

Version 1 does not reconstruct an earlier choice, helper-delivery state, atomic
batch order, or exact unconfirmed transaction after the voting database is
lost. Those are separate transaction-recovery concerns.

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

Persisted state adds explicit authority scheme, root source, authority context,
and bundle-ID columns. Migration assigns `random-v0` to all existing rows. It
never infers a scheme from secret length or recomputes a value merely because a
column is empty.

Changing roots or providers is allowed only before any delegation may have been
submitted. After that point, the original authority remains mandatory.

## Public-target and custody boundary

The existing public-target flow separates a voter who owns the hotkey from a
funds controller who selects and proves the delegation bundles. Version 1 of
this proposal does not silently change that protocol. Its root providers bind
the context to the same account that supplies the root, so they cannot be
applied unchanged when the voter and funds controller own different accounts.

A future deterministic extension must define a separately versioned custody
context and provider contract. It can still reuse the 64-byte root boundary and
all post-provider expansion. The funds controller would send the authenticated
custody context and canonical bundle IDs to the voter, then receive the
corresponding secret blindings over the existing authenticated, confidential
channel. The voter would not need the underlying notes, and the funds
controller would never receive the authority root or hotkey secret. That
additional two-party exchange should be reviewed with the custody handoff as a
separate version.

## Security properties

### Root and fund separation

The software seed provider returns only a round-scoped root, not the root seed,
account spending key, viewing key, or registered subtree key. The current
Keystone path does not export any seed-derived secret at all. Compromise of an
authority root allows voting for its bound context but does not allow spending
ZEC or deriving another Zcash account.

The authority context is included again in both root expansions. Accidental
reuse of one random root across two contexts therefore produces different
hotkeys and blindings, although integrations must still reject such reuse as a
state error. The account fingerprint makes two unrelated seeds at the same
account index distinct contexts and makes a wrong-account backup detectable.

### Privacy and unlinkability

The root is uniformly random or pseudorandom to anyone without the recovery
source. Determinism does not make it publicly guessable. Account, network,
chain, and round separation produce different authorities. Bundle IDs produce
different VAN blindings within one round.

No account fingerprint, authority root, hotkey secret, VAN blinding factor, or
bundle ID is published in transaction memos or vote-chain messages.

### Backup compromise

The `stored-random-v1` backup contains full voting authority for one context.
Its encryption and authentication are security boundaries, not mere data-at-
rest conveniences. A copied plaintext root lets an attacker vote but still
does not let the attacker spend ZEC.

Deleting the live root after delegation does not revoke a copied backup or
change the on-chain authority. Wallet UX must describe backup deletion as a
custody action, not revocation.

### Wallet compromise

A wallet that holds the authority root can vote for that context. This is the
same authority the wallet holds today when it stores the hotkey and VAN
blindings. Seed derivation or hardware-backed Zcash transaction signing does
not turn an actively compromised voting host into a trusted prover.

### Cross-protocol separation

The software source uses its own application-owned registered-KDF context and
numeric namespace. ZIP 325's Account Metadata Key tree is not used as the
voting signing authority. ZIP 325 may be appropriate for encrypting an
off-chain backup, but its current purpose is metadata encryption and it does
not standardize voting authority.

## Rejected alternatives

- **Require deterministic Keystone authority now:** a purpose-specific private
  output needs new firmware and transport. A backed-up host-generated root does
  not.
- **Use an ordinary signature or deterministic sign-message request:** current
  RedPallas signatures are randomized, and either approach couples derivation
  to a signing protocol. A future device should return the typed root directly.
- **Reuse today's random hotkey seed as the root:** legacy code interprets those
  bytes as a ZIP-32 seed. Version 1 gives every root one direct-expansion
  meaning and leaves old authorities under `random-v0`.
- **Derive blindings from the hotkey or public key:** a signing key should not
  become a general KDF key, and a public derivation would reveal the blind.
- **Derive blindings from only the bundle index:** the same index can identify
  different notes after a planning change, so the actual bundle identity must
  be bound.
- **Back up every sampled blinding:** the backup grows with the plan and can
  capture only a partial plan. One root recovers every canonical bundle.
- **Store the root only on Keystone:** the host needs it for vote construction,
  and device-only state creates another backup problem.
- **Use ZIP 325 as the authority:** [ZIP 325][zip325] describes metadata
  encryption, not a voting-signing namespace. It may still protect a backup.

[zip32-registered]: https://zips.z.cash/zip-0032#specification-registered-key-derivation
[zip325]: https://zips.z.cash/zip-0325

## Repository responsibilities

### `zcash_voting`

- account fingerprint, authority context, root, source, and backup types;
- hotkey, bundle-ID, and VAN-blinding derivation;
- the recoverable bundle policy and round-auth version 3 verification;
- legacy migration and current-VAN tree recovery; and
- atomic-batch rules, public vectors, and integration fixtures.

### Software-wallet integrations

- root-seed provider implementation;
- secure root handling and backup where applicable;
- snapshot rescan and recovery UX; and
- fail-closed handling for unsupported schemes or custom non-recoverable note
  selection.

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

No circuit change is required. Builder documentation that currently requires
`van_comm_rand` to be sampled directly from a CSPRNG must also permit a
domain-separated PRF output that is uniformly mapped into the Pallas base
field. Cross-repository fixtures should prove that the derived hotkey and VAN
blinding factor produce the expected public VAN and valid ZKP #1 and ZKP #2
statements.

### `vote-sdk`

No consensus rule or vote-action message change is required for authority
derivation. `vote-sdk` must add the round-auth version 3 config fields,
canonical signing preimage, signer and config-PR support, and verification
fixtures shared with `zcash_voting`. An explicit on-chain scheme field may be
considered later, but is not needed when round-auth version 3 is enforced.

## Validation and rollout gates

Before activation, require:

- public vectors for the account fingerprint, software root provider, hotkey,
  bundle ID, VAN blinding, and VAN;
- separation tests for account, network, vote chain, round, bundle identity,
  and version, including invalid hotkey candidates;
- random-root generation, encrypted backup, restore, wrong-context,
  wrong-account, corruption, and backup-gate tests;
- round-auth version 3 vectors, signature-tampering coverage, and old-client
  rejection;
- proof verification through both supported circuit backends;
- database-loss recovery before delegation, after partial delegation, and
  after confirmed singleton and atomic votes, including invalid or
  incomparable tree matches;
- Keystone-host recovery with existing PCZT batch signing and no new firmware
  request; and
- legacy `random-v0` and downstream integration coverage.

Rollout order:

1. Land the normative `zcash_voting` types, encodings, recovery, and vectors
   without activating the scheme.
2. Implement the software seed provider and `stored-random-v1` provider.
3. Land round-auth version 3 production and verification in `vote-sdk`.
4. Land downstream backup and recovery UX.
5. Validate the existing Keystone PCZT batch-signing integration with restored
   authority roots.
6. Activate `authority-root-v1` only in authenticated round configuration that
   excludes unsupported wallet versions.

No firmware release is part of this rollout order.

## Review focus

The main decisions are the 64-byte root boundary, account and bundle binding,
software-derived versus backed-up random roots, signed version selection, and
forward-only legacy migration. Version 1 changes no circuit, vote-chain
message, or Keystone firmware behavior. Public-target custody derivation
remains a separate follow-up.
