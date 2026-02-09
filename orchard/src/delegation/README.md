# Delegation Circuit (ZKP 1)

## Inputs

- Public
   * **nf_signed**: a unique, deterministic identifier derived from a note's secret components that publicly marks the note as spent.
   * **rk**: the randomized public key for spend authorization. Derived per-transaction, publicly exposed, unlinkable, paired with `rsk` - the private key
   * **gov_comm**: the governance commitment — a Pallas base field element identifying the governance context (e.g., a particular DAO or proposal framework). Scopes the delegation proof to a specific governance domain, preventing cross-governance replay.
   * **vote_round_id**: the vote round identifier — a Pallas base field element identifying the specific voting round or epoch. Prevents cross-round replay: a keystone signature for round N cannot be reused in round N+1.

- Private
   * **ρ** "rho": The nullifier of the note that was spent to create the signed note
   * **ψ** ("psi"): A pseudorandom field element derived from the note's random seed `rseed` and its nullifier domain separator rho
   * **cm**: The note commitment, witnessed as an ECC point
   * **nk**: nullifier key
   * **ak**: spend authorization validating key (the long-lived public key for spend authorization)
   * **alpha**: a fresh random scalar used to rerandomize the spend authorization key for each transaction.
   * **rivk**: is the randomness (blinding factor) for the CommitIvk Sinsemilla commitment. The name stands for randomness for incoming viewing key.
   * **rcm_signed**: the note commitment trapdoor (randomness). A scalar derived from `rseed` and `rho` that blinds the commitment.
   * **g_d_signed**: the diversified generator from the note recipient's address
   * **pk_d_signed**: the diversified transmission key from the note recipient's address
   * **cmx_1, cmx_2, cmx_3, cmx_4**: the extracted note commitments (`ExtractP(cm_i)`) of the four notes being delegated. Each is a Pallas base field element (x-coordinate of the commitment point). Hashed together with `gov_comm` and `vote_round_id` to produce `rho_signed` in condition 3. Currently free witnesses; a future condition (condition 10) will derive them in-circuit.

## 1. Signed Note Commitment Integrity

Purpose: ensure that the signed note commitment is correctly constructed. This establishes the link between spending authority, nullifier key and the note itself

What it proves:

The circuit recomputes the note commitment in-circuit from the note's witness data and constrains the result equal to the witnessed commitment `cm_signed`.

Establishes the binding link between `ak`, `nk` and the note itself `cm`

```
NoteCommit_rcm_signed(repr(g_d_signed), repr(pk_d_signed), 0, rho_signed, psi_signed) = cm_signed
```

Where
- **rcm_signed**: this is the note commitment randomness (also called the trapdoor). It is a scalar derived from the note's `rseed` and `rho`. It blinds the commitment so that two notes with identical contents produce different commitments. It appears as a subscript because of how Pedersen/Sinsemilla commitments work structurally:
`Commit_r(m) = Hash(m) + [r] * R`. So, expanded, the formula is really:
`cm_signed = SinsemillaHash(repr(g_d_signed) || repr(pk_d_signed) || 0 || rho_signed || psi_signed) + [rcm_signed] * R`
- **repr(g_d_signed)** - The diversified base point from the recipient's payment address. `g_d` is a point on the Pallas curve derived deterministically from the address's diversifier d. `repr()` extracts its canonical byte representation (the x and y coordinates). It ensures the commitment is bound to a specific diversified address. This value is witnessed privately and also used in the address integrity check (`pk_d = [ivk] * g_d`).
- **0**: The note value is hardcoded to zero since the "signed note" in this delegation context is always a dummy/zero-value note.
- **ρ** ("rho"): The nullifier of the note that was spent to create this note. It is a Pallas base field element that serves as a unique, per-note domain separator. rho ensures that even if two notes have identical contents, they will produce different nullifiers because they were created by spending different input notes. rho provides deterministic, structural uniqueness — it chains each note to its creation context. A single tx can create multiple output notes from the same input; all those outputs share the same rho. If nullifier derivation only used rho (no psi), outputs from the same input could collide.
- **ψ** ("psi"): A pseudorandom field element derived from the note's random seed `rseed` and its nullifier domain separator rho. It adds randomness to the nullifier so that even if two notes share the same rho and nk, they produce different nullifiers. We provide it as a witness instead of deriving in-circuit since derivation would require an expensive Blake2b. psi provides randomized uniqueness — it is derived from `rseed` which is freshly random per note. Even if multiple outputs are derived from the same note, different `rseed` values produce different psi values. But if uniqueness relied only on psi (i.e. only randomness), a faulty RNG would cause nullifier collisions. Together with rho, they cover each other's weaknesses. Additionally, there is a structural reason: if we only used psi, there would be an implicit chain where each note's identity is linked to the note that was spent to create it. The randomized psi breaks the chain, unblocking a requirement used in Orchard's security proof.
- **cm_signed** The witnessed note commitment, the value the prover claims is the commitment for this note. The circuit recomputes `NoteCommit` from all the above inputs and then enforces strict equality against this witnessed `cm_signed`. If any single parameter is wrong (wrong address, wrong randomness, wrong rho/psi), the derived commitment won't match and proof creation fails.

In essence, the commitment binds together: **who the note belongs to** (g_d, pk_d), **how much it's worth** (0), **where it came from** (rho), **random uniqueness** (psi), **all blinded by randomness** (rcm).

Note:
- The constraint is strict equality. No null option. If the commitment does not match, proof creation fails.

## 2. Signed Nullifier Integrity

Purpose: Derive the standard Orchard nullifier deterministically from the note's secret components. Validate it against the one we created exclusion proof from.

```
derive nf_signed = DeriveNullifier(nk, rho_signed, psi_signed, cm_signed)
```

Where:  
- **nk**: The nullifier deriving key associated with the note.

- **ρ** ("rho"): The nullifier of the note that was spent to create the signed note. It is a Pallas base field element that serves as a unique, per-note domain separator. rho ensures that even if two notes have identical contents, they will produce different nullifiers because they were created by spending different input notes. rho provides deterministic, structural uniqueness — it chains each note to its creation context. A single tx can create multiple output notes from the same input; all those outputs share the same rho. If nullifier derivation only used rho (no psi), outputs from the same input could collide.

- **ψ** ("psi"): A pseudorandom field element derived from the note's random seed `rseed` and its nullifier domain separator rho. It adds randomness to the nullifier so that even if two notes share the same rho and nk, they produce different nullifiers. We provide it as a witness instead of deriving in-circuit since derivation would require an expensive Blake2b. psi provides randomized uniqueness — it is derived from `rseed` which is freshly random per note. Even if multiple outputs are derived from the same note, different `rseed` values produce different psi values. But if uniqueness relied only on psi (i.e. only randomness), a faulty RNG would cause nullifier collisions. Together with rho, they cover each other's weaknesses. Additionally, there is a structural reason: if we only used psi, there would be an implicit chain where each note's identity is linked to the note that was spent to create it. The randomized psi breaks the chain, unblocking a requirement used in Orchard's security proof.

- **cm**: The note commitment, witnessed as an ECC point (the form `DeriveNullifier` expects). Converted from `NoteCommitment` to a Pallas affine point in-circuit.

**Function:** `DeriveNullifier`

**Type:**  
```
DeriveNullifier: 𝔽_qP × 𝔽_qP × 𝔽_qP × ℙ → 𝔽_qP
```

**Defined as:**  
```
DeriveNullifier_nk(ρ, ψ, cm) = ExtractP(
    [ (PRF_nf_Orchard_nk(ρ) + ψ) mod q_P ] * 𝒦_Orchard + cm
)
```

- `ExtractP` denotes extracting the base field element from the resulting group element.  
- `𝒦_Orchard` is a fixed generator. Input to the `EccChip`.
- `PRF_nf_Orchard_nk(ρ)` is the nullifier pseudorandom function as defined in the Orchard protocol. Uses Poseidon hash for PRF.

**Constructions**:
- `Poseidon`: used as a PRF function.
- `Sinsemilla`: provides infrastructure for the lookup tables of the ECC chip.


- **Why do we take PRF of rho?**
   * The primary reason is unlinkability. Rho is the nullifier of the note that was spend to create this note. In standard Orchard, nullifiers are published onchain. The PRF destroys the link.
- **Why not expose nf_old publicly?**
   * In standard Orchard, the nullifier is published to prevent double-spending. In this delegation circuit, nf_old is not directly exposed as a public input. Instead, it is checked against the exclusion interval and a domain nullifier is published instead. The standard nullifier stays hidden.

## 3. Rho Binding

Purpose: the signed note's rho is bound to the exact notes being delegated, the governance commitment, and the round. This is what makes the keystone signature non-replayable and scoped.

```
rho_signed = Poseidon(cmx_1, cmx_2, cmx_3, cmx_4, gov_comm, vote_round_id)
```

Where
- **cmx_1, cmx_2, cmx_3, cmx_4**: The extracted note commitments (`ExtractP(cm_i)`) of the four notes being delegated. Each `cmx_i` is a Pallas base field element — the x-coordinate of the corresponding note's commitment point. By hashing all four commitments into rho, the keystone signature is bound to the exact set of notes the delegator chose. Tampering with any single commitment changes the hash and invalidates the proof. Currently witnessed as free private inputs; a future condition (condition 10) will derive them in-circuit from the actual note data.
- **gov_comm**: The governance commitment — a Pallas base field element identifying the governance context.
- **vote_round_id**: The vote round identifier — a Pallas base field element identifying the specific voting round or epoch.

**Function:** `Poseidon` with `ConstantLength<6>`

Uses the same `Pow5Chip` / `P128Pow5T3` construction as the nullifier derivation, but with 6 inputs instead of 2. With rate 2, the sponge absorbs 2 elements per permutation round (3 absorption rounds for 6 inputs). The domain separator includes the input length, providing proper cryptographic separation from other Poseidon uses in the circuit.

**Constraint:** The circuit computes `derived_rho = Poseidon(cmx_1, cmx_2, cmx_3, cmx_4, gov_comm, vote_round_id)` and enforces strict equality `derived_rho == rho_signed`. Since `rho_signed` is the same value used in both note commitment integrity (condition 1) and nullifier integrity (condition 2), this creates a three-way binding: the nullifier, the note commitment, and the delegation scope are all tied to the same rho.

## 4. Spend Authority

Purpose: proves spending authority while preserving unlinkability. Links to the Keystone spend-auth signature out-of-circuit.
- Only the holder of `ask` can produce `rsk = ask + alpha` and sign under `rk`, proving they are authorized to spend the note.
- `alpha` is fresh randomness each time, the published `rk` reveals nothing about `ak` - different spends from the same wallet cannot be correlated by observers.

```
rk = SpendAuthSig.RandomizePublic(alpha, ak) 
```
i.e. rk = ak + [alpha] * G

Where:
- `ak` - the authorizing key, the long-lived public key for spend authorization.
- `alpha` - the fresh randomness published each time. If rk were the same across transactions, an observer could link them to the same spender.
- `G` - the fixed base generator point on the Pallas curve dedicated to the spend authorization.

Spend Authority: i.e. `rk = ak + [alpha] * G` — the public `rk` is a valid rerandomization of `ak`. Links to the Keystone signature verified out-of-circuit.

## 5. Diversified Address Integrity

Purpose: proves the signed note's address belongs to the same key material `(ak, nk)`. This is where `ivk` is established — it will be reused for every real note ownership check.

Without address integrity, the nullifier integrity proves:
- "I know (nk, rho, psi, cm) that produce this nullifier"
- "I know ak such that rk = ak + [alpha] * G".

But there is nothing that ties ak to nk. They are witnessed independently. A malicious prover could:
- Supply their own `ak` (i.e passes spend authority, can sign under `rk`)
- Supply someone else's `nk` (i.e. valid nullifier for someone else's note)

```
ivk = ⊥  or  pk_d_signed = [ivk] * g_d_signed
where ivk = CommitIvk_rivk(ExtractP(ak_P), nk)
```

What address integrity fixes:
- `CommitIvk(ExtractP(ak), nk)` forces `ak` and `nk` to come from the same key tree
- `pk_d_signed = [ivk] * g_d_signed` proves the note's destination address was derived from this specific ivk. This will be asserted on as part of validating note commitment integrity.

The `ivk = ⊥` case is handled internally by `CommitDomain::short_commit`: incomplete addition allows the identity to occur, and synthesis detects this edge case and aborts proof creation. No explicit conditional is needed in the circuit.

Where:
- **ak_P** — the spend validating key (shared with spend authority). `ExtractP(ak_P)` extracts its x-coordinate.
- **nk** — the nullifier deriving key (shared with nullifier integrity).
- **rivk** — the CommitIvk randomness, extracted from the full viewing key via `fvk.rivk(Scope::External)`. Note that it is derived once at key creation time and is static.
- **g_d_signed** — the diversified generator from the note recipient's address.
- **pk_d_signed** — the diversified transmission key from the note recipient's address.

**Constructions:**
- `CommitIvkChip` — handles decomposition and canonicity checking for the CommitIvk Sinsemilla commitment.
- `SinsemillaChip` — the same instance used for lookup tables is reused for CommitIvk.

## TODO

- Better understand underlying Poseidon and AddChip constructions. Specifically, how they select columns.
- Understand Sinsemilla construction and why it well-suited for Pallas.
