# Recoverable voting authority design

## Status

This document is a proposal for review. It does not describe behavior that is
implemented today.

The review goal is to approve the authority model, derivation boundaries,
hardware-wallet flow, recovery contract, and migration boundary before code is
written. Approval fixes the version 1 decisions below. Implementation must
freeze their byte-for-byte encodings and public test vectors in `zcash_voting`;
production activation additionally requires an assigned ZIP number.

## Executive summary

A voter currently needs two randomly generated secrets after delegation:

- the voting hotkey, which authorizes later votes; and
- `van_comm_rand`, the secret blinding factor for each Vote Authority Note
  (VAN). Some existing code calls the same value `gov_comm_rand`.

If a delegation reaches the vote chain and the wallet later loses either
secret, restoring the wallet seed is not enough to recreate that VAN. The
voter can still see that their Zcash account participated, but cannot prove
control of the voting authority that was placed on-chain.

This proposal replaces those independent random secrets with one deterministic,
round-scoped **voting capability**. A software wallet derives the capability
locally from its wallet seed. A Keystone derives the same capability on-device
and returns only that one round-scoped secret to the wallet in an encrypted QR
response. `zcash_voting` then expands the capability into the voting hotkey and
one independent VAN blinding factor per canonical delegation bundle.

```text
Software wallet seed -- local registered KDF --+
                                                |
                                                +--> round voting capability
                                                |             |
Keystone seed -------- device registered KDF --+             v
                                                        zcash_voting
                                                     /                \
                                           voting hotkey        bundle VAN blinds
```

The capability cannot spend ZEC. Possession authorizes voting only for one
account, network, vote chain, and round. Different accounts and rounds derive
unlinkable capabilities.

The change is client-side. The voting circuits and vote-chain verification do
not need to know how the private witnesses were derived.

### Recovery at a glance

| Material available after loss | `registered-v1` recovery path |
| --- | --- |
| Original software-wallet seed or mnemonic | Re-derive the capability locally |
| Keystone restored from the same seed | Approve one encrypted capability export |
| UFVK or watch-only wallet | Cannot derive voting authority |
| Account-level Unified Spending Key only | Cannot derive the registered root |
| Legacy `random-v0` round | Requires the original secrets or a full-round backup |

## Motivating failure

Consider a voter whose snapshot notes produce three delegation bundles:

1. Bundle 0 is signed and confirmed on the vote chain.
2. The app crashes or its voting database is lost before all local recovery
   state is durable.
3. The user restores the same Zcash wallet.

Today the restored app samples a different hotkey and different VAN blinding
factor. It therefore computes a different VAN for bundle 0. Rebuilding the
delegation transaction does not help because the original eligible notes may
already be represented by the confirmed delegation, and later vote proofs must
use the hotkey and blinding factor committed by that original VAN.

A transaction hash is not a durable identity for this recovery. Other valid
transaction and proof randomness can change when work is rebuilt. Recovery
must recompute the original VAN and locate that commitment in a root-validated
vote tree.

## Goals

- Restore the same voting hotkey from the same wallet seed, account, network,
  vote chain, and round.
- Restore a different, deterministic VAN blinding factor for every canonical
  delegation bundle.
- Give software wallets and Keystone wallets the same derivation result and
  the same `zcash_voting` API after the seed boundary.
- Keep the wallet root seed, account spending key, and broader registered-key
  subtrees out of `zcash_voting` and off the QR channel.
- Preserve one fresh, unlinkable voting authority per account and round.
- Recover confirmed partial delegation from VAN commitments rather than exact
  transaction bytes.
- Version every persisted and wire-level derivation artifact so legacy random
  rounds continue to work without reinterpretation.
- Consolidate the normative encoding, expansion, bundle identity, recovery,
  documentation, and test vectors in `zcash_voting`.

## Non-goals

- Recover random secrets that were already lost in a legacy round.
- Change the ZKP statements, on-chain messages, or vote-chain verifier.
- Move ZKP generation into Keystone.
- Give Keystone an arbitrary registered-key or wallet-secret export API.
- Remove the normal user confirmation required before a hardware wallet
  releases voting authority.
- Make an imported UFVK or account-level Unified Spending Key sufficient to
  reconstruct the registered tree. Version 1 recovery starts from the original
  wallet seed or mnemonic.
- Guarantee recovery for a manually selected note subset or custom bundle
  policy that is not itself recoverably described.
- Replace the existing public-target custody handoff in the first version.
  A future version can add a two-party exchange for deterministic bundle
  blindings without changing the self-custody design in this document.

## Terms

### Voting hotkey

An Orchard `SpendingKey` used only by the shielded-voting protocol. Its address
is the delegation target and its spend-authorizing key signs later vote-chain
actions. It cannot spend the Zcash account that delegated the voting weight.

### VAN blinding factor

The Pallas base-field element used to hide the address and weight in a VAN
commitment. The storage and delegation code call it `van_comm_rand`. Some vote
builder APIs call the same value `gov_comm_rand`; version 1 should use the VAN
name consistently.

### Round voting capability

A 64-byte secret derived for exactly one Zcash account, network, vote chain,
and voting round. It is the narrowest secret transferred across the software
or hardware seed boundary. `zcash_voting` expands it into the hotkey and bundle
blindings but cannot use it to derive another account, round, or wallet key.

## Design decisions

### 1. Use ZIP 32 Registered Key Derivation

The seed-owning component derives the round capability using
[ZIP 32 Registered Key Derivation][zip32-registered]. A dedicated
shielded-voting ZIP and globally unique context string must be assigned before
production deployment.

Conceptually, the tree is:

```text
RegKD("ZcashShieldedVoting", wallet_seed, SHIELDED_VOTING_ZIP)
    / coin_type(network)'
    / account'
    / round-capability=0'(network, vote_chain_id, vote_round_id)
```

The coin-type and account elements use empty tags. `coin_type(network)` is 133
for mainnet and 1 for testnet or regtest, matching the account path used by the
wallet. The account-level registered key is never exported. The final operation
uses `derive_child_cryptovalue` at hardened child index zero to produce one
64-byte leaf for the round. The network, vote-chain identifier, and round
identifier are encoded in that child's tag.

The version 1 tag encoding is:

```text
0x01
|| network_tag
|| vote_chain_id_length_u16_le
|| vote_chain_id_utf8
|| vote_round_id_32
```

`network_tag` is `0` for mainnet, `1` for testnet, and `2` for regtest.
`vote_chain_id` must first pass the crate's canonical chain-ID validation.
`vote_round_id_32` is the canonical little-endian 32-byte Pallas field encoding
used by the round. The ZIP-32 account index must be below `2^31`.

An implementation may use a clearly marked provisional ZIP number for local
experiments, but it must not publish interoperable production vectors or ship
the derivation until the context and number are assigned. Changing either one
changes every derived authority.

### 2. Keep the root seed outside `zcash_voting`

`zcash_voting` owns a canonical `VotingCapabilityRequestV1` descriptor. That
descriptor contains the registered context, assigned ZIP number, account path,
and round tag. A seed provider executes the standard registered KDF and returns
only `VotingRoundCapabilityV1`.

This produces two equivalent providers:

- a software provider calls the registered KDF locally; and
- a Keystone provider sends the descriptor's typed round context to firmware.

The normal wallet-facing API does not accept a mnemonic, root seed, account
spending key, or registered subtree key. A reference helper may exist behind a
narrow seed-provider boundary so test vectors and software integrations do not
duplicate the registered path.

### 3. Derive the hotkey directly from the capability

The 64-byte registered cryptovalue must not be passed into any ZIP-32 KDF. In
particular, it must not be treated as a seed for
`UnifiedSpendingKey::from_seed`, which is how today's random 64-byte hotkey
secret is interpreted.

Version 1 derives a direct Orchard spending key instead:

```text
candidate(i) = BLAKE2b-256(
    key = round_capability_64,
    personalization = "ZcashVoteHotkey",
    message = i_u32_le,
)
```

Starting with `i = 0`, interpret `candidate(i)` with
`Orchard::SpendingKey::from_bytes`. Increment `i` until the result is valid.
The expected number of iterations is one; the counter makes the function
interoperable even for a rare invalid candidate. Return a derivation error
rather than wrapping if the `u32` counter is exhausted.

The hotkey uses external Orchard address index zero. The persisted secret is a
versioned typed value, not an untagged byte string whose meaning is guessed
from its length:

```text
VotingHotkeySecret::RandomV0(64 bytes)
VotingHotkeySecret::RegisteredV1(32-byte Orchard SpendingKey)
```

Both variants remain zeroized in memory and storage adapters continue to treat
them as voting authority.

### 4. Bind each VAN blinding factor to a canonical bundle identity

A round can have multiple delegation bundles that share one hotkey. Each bundle
must receive independent blinding material. Bundle index alone is not enough:
an SDK policy change could assign different notes to the same index after a
database loss.

`zcash_voting` defines `VotingBundleIdV1` before any random padding notes, PCZT
fields, proofs, or transaction bytes are created:

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

The note commitment must be its canonical 32-byte encoding. Canonical bundle
order is the order returned by the version 1 snapshot-selection and bundle
planning policy. Padding notes are excluded because their randomness is not
recoverable and they do not define the real voting weight.

The blinding factor is then:

```text
wide = BLAKE2b-512(
    key = round_capability_64,
    personalization = "ZcashVoteVANv1",
    message = bundle_id,
)

van_comm_rand = PallasBase::from_uniform_bytes(wide)
```

The capability, rather than the derived hotkey bytes, is the key for this
expansion. The hotkey and VAN blindings are sibling outputs with separate
domains. This avoids turning an Orchard signing key into a general-purpose KDF
key and keeps the bundle derivation stable if the hotkey representation changes
in a future scheme.

This mapping avoids biased reduction and always returns a canonical field
element. The bundle ID may be persisted for diagnostics and conflict checks,
but it is privacy-sensitive metadata and is not published on-chain.

### 5. Freeze the recoverable note-selection policy

Deterministic secrets are necessary but not sufficient if a restored wallet
cannot reconstruct the original bundle weights. Version 1 recovery therefore
uses a named, frozen snapshot-note selection and `BundlePolicy` definition
owned by `zcash_voting`.

The authenticated round configuration identifies:

- `voting_authority_scheme = "registered-v1"`; and
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
tell the user that seed-only voting recovery is unavailable. They must not be
labelled `recoverable-v1`.

An intentionally skipped suffix of the canonical bundle plan does not change
earlier bundle identities. Recovery can discover which prefix reached the
vote chain and let the user make a new decision about bundles that were never
submitted.

### 6. Select the scheme through authenticated round configuration

Rounds created before activation do not contain a scheme and remain
`random-v0`. New recoverable rounds explicitly select `registered-v1` in the
authenticated configuration consumed by `zcash_voting`.

The crate fails closed on an unknown scheme or policy. A wallet must not decide
between random and deterministic authority from an application version, date,
secret length, or whether local rows happen to exist.

The vote chain does not need the scheme to verify proofs or signatures. A
future vote-chain field may make capability negotiation more visible, but it
is not required for the first implementation if the existing authenticated
round configuration carries the value.

Deployment must still prevent an older wallet that ignores the new field from
joining a `registered-v1` round under random behavior. The configuration switch
should therefore be activated only for wallet versions that require and
understand the scheme.

## Software-wallet flow

### Initial delegation

1. Resolve and authenticate the round configuration.
2. Reconstruct the canonical snapshot note set and `recoverable-v1` bundle
   plan.
3. Ask the software seed provider for the registered round capability.
4. Construct the hotkey and every bundle ID and blinding factor in
   `zcash_voting`.
5. Persist the scheme, hotkey secret, bundle IDs, blindings, and plan for normal
   operation. Persistence is a cache and resume optimization, not the only
   backup.
6. Build proofs, sign, submit, and confirm delegation through the existing
   lifecycle.

### Recovery

After a fresh install, the same seed provider and round context produce the
same capability. The wallet can reconstruct the plan and recover on-chain VANs
without a separate voting-secret backup.

A watch-only wallet, imported UFVK, or account-level Unified Spending Key cannot
derive the capability because none contains the registered root. That is an
intentional authority boundary. Recovery requires the original wallet seed or
mnemonic; otherwise voting authority must arrive through an external handoff or
an encrypted backup.

## Keystone flow

### Why the existing transaction signature is not the KDF

The current Keystone Orchard signing path uses fresh randomness for RedPallas
signatures, so signing the same bytes again does not reproduce the same
signature. Making that signature deterministic would still not solve the
ordering problem: the delegation PCZT already contains the hotkey address and
VAN that would supposedly be derived from its signature.

A separate derivation request is therefore required before delegation PCZTs
are built. It is a key-derivation operation, not a synthetic transaction or
message signature.

### Typed request and encrypted response

Define these dedicated UR registry types:

- `zcash-voting-key-request`; and
- `zcash-voting-key`.

`zcash_voting` owns the host-side request and response model, deterministic CBOR
encoding, UR type names, and golden bytes. Keystone mirrors that encoding and
the shared vectors rather than defining a second wallet-specific format.

The request contains:

- a 16-byte random request ID;
- derivation scheme version;
- 32-byte expected Zcash seed fingerprint;
- ZIP-32 account index;
- network;
- vote-chain identifier;
- canonical 32-byte vote-round identifier; and
- a 32-byte HPKE recipient public key generated by the wallet for this request.

The firmware supports only the registered shielded-voting descriptor. The host
cannot supply an arbitrary context, ZIP number, child path, or export scope.
Firmware rejects a request whose expected seed fingerprint does not match the
selected wallet seed.

After the user approves, firmware derives the final round capability leaf and
encrypts it to the wallet with the [RFC 9180][rfc9180] base-mode suite:

```text
DHKEM(X25519, HKDF-SHA256)
HKDF-SHA256
ChaCha20-Poly1305
```

The canonical request bytes are authenticated as associated data. The response
echoes the request ID and scheme version and carries the HPKE encapsulated key
and ciphertext. HPKE uses the ASCII bytes `ZcashVotingKeyV1` as its `info` value
and the complete canonical request bytes as associated data. The encrypted
plaintext contains the 64-byte capability, firmware-version string, and a
randomized RedPallas signature by the selected account's unrandomized Orchard
SpendAuthorizingKey. The signed digest is:

```text
auth_digest = BLAKE2b-256(
    personalization = "ZcashVoteAuthV1",
    message = canonical_request_bytes
              || firmware_version_length_u16_le
              || firmware_version_utf8
              || round_capability_64,
)
```

The canonical request includes the ephemeral HPKE recipient key, so the
signature binds the capability to this request and recipient. The wallet
verifies the signature against the Orchard SpendValidatingKey in its
independently held UFVK before handing the capability to `zcash_voting`. This
domain is distinct from a Zcash transaction signature digest.

The wallet accepts a response only for one outstanding request ID and ephemeral
recipient key, then deletes the recipient private key whether the request is
accepted or cancelled.

HPKE base mode provides confidentiality but does not by itself authenticate the
sender: anyone who sees the wallet's recipient public key could encrypt a
substitute capability. The account signature prevents that substitution from
delegating the vote to attacker-known material. It is an authentication result,
not derivation entropy, and may remain randomized.

The capability is never shown in plaintext QR data, logs, crash reports, or UI
diagnostics. Firmware zeroizes the capability, account-level registered key,
HPKE shared secret, and seed-backed intermediates after encoding the response.

Encryption protects the response from cameras and unrelated QR readers. It
does not make an approved malicious wallet harmless: the requesting wallet
owns the recipient key and will receive the capability. Human-readable device
review remains the authorization boundary.

### Device review

The confirmation screen identifies:

- the operation as **Export voting authority**;
- Zcash network and account;
- vote-chain identifier;
- a short fingerprint of the round ID;
- that the authority can vote in this round but cannot spend ZEC; and
- that the requesting wallet will receive the round-scoped secret.

A host-provided round title may be shown as unverified convenience text. The
canonical round fingerprint must remain visible because the device cannot
independently authenticate the title.

### Delegation and recovery interactions

Initial delegation requires two hardware interactions:

1. derive and export the encrypted round capability; then
2. sign the completed delegation PCZTs using the existing Zcash batch-signing
   request.

The first result is needed before the wallet can compute the hotkey, VANs, and
ZKP #1 inputs, so these interactions cannot be collapsed without moving proof
generation or transaction construction onto Keystone. Multiple delegation
PCZTs remain one batch-signing interaction and do not become dependent
on-chain transactions.

Recovery requires only the first interaction. Keystone does not need to keep a
per-round record in secure storage: the same restored device seed derives the
same capability on demand.

The firmware's existing public HD-key request may provide reusable parsing and
UI plumbing, but it must not be extended to return private material generically.

## Recovery state machine

Given the restored account seed or Keystone seed and independently
authenticated round configuration, `zcash_voting` recovery proceeds as follows:

1. Rescan the account at the round's snapshot height, including notes that are
   spent at the current tip but were eligible at the snapshot.
2. Reconstruct the frozen `recoverable-v1` bundle plan.
3. Re-derive the round capability, hotkey, bundle IDs, blinding factors, weights,
   and candidate VAN commitments.
4. Sync and root-validate the public vote-commitment tree.
5. Locate every candidate VAN in that tree.
6. Atomically import only unambiguous matches with their confirmed positions.
7. Continue normal delegation for a candidate with no match only after the
   wallet establishes that its source notes remain usable and no ambiguous
   broadcast can still land.
8. Reject duplicate, conflicting, partial, or root-inconsistent recovery state.

A confirmed VAN is authoritative even if the original transaction hash is
missing. A transaction hash remains useful for normal polling but does not
replace commitment recovery.

Once voting has begun, the same hotkey and VAN blinding factor reconstruct the
authority transitions. Existing vote recovery continues to use confirmed
vote-commitment positions and proposal-authority state. One-time proof and
signature randomizers may be freshly generated when unsubmitted work is
rebuilt.

## Legacy migration

Existing rows and rounds remain `random-v0`:

- their 64-byte stored hotkey secret keeps its current interpretation;
- their persisted `van_comm_rand` values remain authoritative; and
- no code attempts to replace them with version 1 derivations.

An on-chain VAN cannot be migrated to a new hotkey or blinding factor. If the
legacy secrets are already gone, version 1 cannot recover them retroactively.

As a short-term mitigation, a wallet that still has a live legacy round may
offer an encrypted full-round backup containing the hotkey secret and every
bundle blinding factor. The existing delegation capability package is not such
a backup because it deliberately excludes the hotkey secret.

Persisted state adds explicit scheme and bundle-ID columns. Migration assigns
`random-v0` to all existing rows. It never infers a scheme from secret length
or recomputes a value merely because a column is empty.

## Public-target and custody boundary

The existing public-target flow separates a voter who owns the hotkey from a
funds controller who selects and proves the delegation bundles. Version 1 of
this proposal does not silently change that protocol.

A future deterministic extension can use the same round capability, but the
funds controller must first send canonical bundle IDs to the voter and receive
the corresponding secret blindings over its existing authenticated,
confidential channel. The voter does not need the underlying notes, and the
funds controller never receives the round capability or hotkey secret. That
additional two-party exchange should be reviewed with the custody handoff as a
separate version.

## Security properties

### Seed and fund separation

The exported capability does not reveal the root seed, account spending key,
full viewing key, account metadata key, or a registered subtree key. Compromise
allows voting only for the bound round and account context. It does not allow
the attacker to spend ZEC or derive another round.

### Privacy and unlinkability

The capability is pseudorandom to anyone without the wallet seed. Determinism
does not make it publicly guessable. Account, network, chain, and round domain
separation produce different hotkeys. Bundle IDs produce different VAN
blindings within one round.

No capability, hotkey secret, VAN blinding factor, or bundle ID is published
in transaction memos or vote-chain messages.

### Wallet compromise

A wallet that holds the decrypted round capability can vote for that round.
This is the same authority the wallet holds today when it stores the hotkey and
VAN blindings. Hardware derivation improves backup and scope; it does not turn
an actively compromised voting host into a trusted prover.

### Cross-protocol separation

The design requires its own registered context and ZIP. ZIP 325's Account
Metadata Key tree is not used as the voting signing authority. ZIP 325 may be
appropriate for encrypting off-chain backups, but its current purpose is
metadata encryption and it does not standardize a voting authority.

## Rejected alternatives

### Derive from an ordinary Keystone transaction signature

Rejected because current RedPallas signatures are randomized, the delegation
transaction already depends on the hotkey, and signature determinism is a
fragile cross-vendor KDF contract.

### Add a deterministic sign-message request

Rejected as the primary design because it would still be a separate hardware
interaction while coupling key derivation to signature encoding. A registered
KDF communicates the actual purpose and has a cleaner scope.

### Feed the registered cryptovalue into `UnifiedSpendingKey::from_seed`

Rejected because ZIP 32 explicitly prohibits using a full-width registered
cryptovalue as input to a ZIP-32 KDF. The capability derives a direct Orchard
spending key instead.

### Derive from an account Unified Spending Key

Rejected for version 1 because treating a serialized account key as new root
entropy would not be ZIP 32 Registered Key Derivation and would couple the
voting protocol to a composite key format. The dedicated registered tree gives
the application an independently scoped key under a globally assigned context.
The resulting tradeoff is explicit: root-seed restoration works, while an
account-key-only import does not. Supporting the latter would require a
separately specified derivation version.

### Derive every bundle blind from only its index

Rejected because a bundle-planning change could assign different notes and
weight to the same index after restoration. The canonical bundle identity
binds the blinding to the actual snapshot notes.

### Derive a blind from public hotkey material

Rejected because an observer could recompute the supposedly secret blind and
brute-force VAN contents.

### Store the hotkey only in Keystone secure storage

Rejected because later ZKP #2 construction currently requires the host to
possess the hotkey secret, and device-local persistence would reintroduce a
second backup problem. Deterministic on-demand derivation works on a restored
device without per-round secure-element state.

### Use ZIP 325 directly

Rejected because [ZIP 325][zip325] is a draft metadata-encryption tree, not a
signing authority namespace. The generic registered-KDF mechanism is reusable;
the metadata tree is not.

[rfc9180]: https://www.rfc-editor.org/rfc/rfc9180.html
[zip32-registered]: https://zips.z.cash/zip-0032#specification-registered-key-derivation
[zip325]: https://zips.z.cash/zip-0325

## Repository responsibilities

### `zcash_voting`

- normative derivation descriptor and encodings;
- typed round capability and versioned hotkey secret;
- direct Orchard hotkey expansion;
- canonical bundle ID and VAN blinding expansion;
- recoverable version 1 note-selection and bundle policy;
- authenticated scheme selection;
- persistence migration and legacy separation;
- VAN-based recovery state machine; and
- public golden vectors, documentation, and integration fixtures.

### Software-wallet integrations

- root-seed provider implementation;
- secure in-memory handling and optional platform-secure caching;
- snapshot rescan and recovery UX; and
- fail-closed handling for unsupported schemes or custom non-recoverable note
  selection.

### Keystone firmware

- whitelisted shielded-voting registered derivation;
- typed UR request and HPKE response;
- account, fingerprint, and round-context review;
- zeroization and resource limits; and
- compatibility vectors matching `zcash_voting`.

### `voting-circuits`

No circuit change is required. Cross-repository fixtures should prove that the
derived hotkey and VAN blinding factor produce the expected public VAN and
valid ZKP #1 and ZKP #2 statements.

### `vote-sdk`

No consensus or message change is required for authority derivation. End-to-end
fixtures should verify delegation, partial recovery, and subsequent voting.
An explicit on-chain scheme identifier may be considered later if authenticated
client configuration is not sufficient for rollout coordination.

## Validation and rollout gates

Before activation, require:

- frozen test vectors from a public test seed through registered capability,
  hotkey secret, raw Orchard address, bundle ID, VAN blinding factor, and VAN;
- software and Keystone implementations producing byte-identical vectors;
- separation tests for account, network, vote chain, round, bundle index, note
  identity, and derivation version;
- invalid-hotkey-candidate counter coverage;
- HPKE round trips, tampering, replay, wrong-request, wrong-account, cancellation,
  forged-account-signature, and zeroization tests;
- simulator and real-device Keystone interoperability;
- database-loss recovery with no delegation, one of several delegations
  confirmed, all delegations confirmed, a missing transaction hash, one vote
  confirmed, and an ambiguous broadcast;
- proof verification through both circuit backends used by supported wallets;
- explicit legacy `random-v0` compatibility and no-silent-migration tests;
- downstream software-wallet integration tests; and
- an assigned shielded-voting ZIP number and published derivation specification.

Rollout order:

1. Land the normative `zcash_voting` types, encodings, and vectors without
   activating the scheme.
2. Implement and validate the software seed provider.
3. Implement the Keystone UR and firmware path against the same vectors.
4. Land VAN-based deterministic recovery and downstream UX.
5. Activate `registered-v1` only in authenticated round configuration that
   excludes unsupported wallet versions.

## Approval checklist

Reviewers are asked to approve or reject these decisions explicitly:

- one deterministic round capability as the shared software/hardware boundary;
- recovery from the original wallet seed, with UFVK and account-level Unified
  Spending Key imports outside the version 1 guarantee;
- ZIP 32 Registered Key Derivation under a new shielded-voting ZIP rather than
  ZIP 325 or signature-derived entropy;
- direct Orchard hotkey derivation instead of re-seeding ZIP 32;
- per-bundle blindings bound to canonical snapshot bundle identities;
- one frozen recoverable note-selection and bundle policy for version 1;
- an encrypted, user-approved Keystone export followed by the existing batch
  signing interaction;
- authenticated round configuration selecting the derivation scheme;
- forward-only migration with all existing rounds remaining `random-v0`;
- no circuit or consensus change for the first version; and
- public-target custody determinism as a separately versioned follow-up.

Approval of this document fixes the version 1 design so implementation PRs can
proceed against it. It does not by itself assign a ZIP number, activate the
scheme for a production round, or approve release of firmware or wallet
changes.
