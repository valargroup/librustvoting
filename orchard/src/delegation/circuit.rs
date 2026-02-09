//! The Delegation circuit implementation.
//!
//! Proves nullifier integrity, spend authority, and diversified address integrity
//! for a single note:
//! - Given private witness data `(nk, rho_signed, psi_signed, cm_signed)`, derives
//!   `nf_signed` in-circuit and constrains it to match the public input.
//! - Given private witness data `(ak, alpha)`, derives `rk = [alpha] * SpendAuthG + ak`
//!   and constrains it to match the public input. This links the ZKP to the
//!   keystone signature verified out-of-circuit.
//! - Given private witness data `(ak, nk, rivk, g_d_signed, pk_d_signed)`, derives
//!   `ivk = CommitIvk_rivk(ExtractP(ak), nk)` and constrains `pk_d_signed = [ivk] * g_d_signed`.
//!   This proves the note's address belongs to the same key material.
//!
//! Follows the 1-circuit-per-note pattern from the vote module. For multiple
//! notes, the builder layer creates multiple independent proofs.

use group::{Curve, GroupEncoding};
use halo2_proofs::{
    circuit::{floor_planner, Layouter, Value},
    plonk::{self, Advice, Column, Instance as InstanceColumn},
};
use pasta_curves::{arithmetic::CurveAffine, pallas, vesta};

use crate::{
    circuit::{
        commit_ivk::{CommitIvkChip, CommitIvkConfig},
        gadget::{
            add_chip::{AddChip, AddConfig},
            assign_free_advice, commit_ivk, derive_nullifier, note_commit,
        },
        note_commit::{NoteCommitChip, NoteCommitConfig},
    },
    constants::{OrchardCommitDomains, OrchardFixedBases, OrchardFixedBasesFull, OrchardHashDomains},
    keys::{
        CommitIvkRandomness, DiversifiedTransmissionKey, FullViewingKey, NullifierDerivingKey,
        Scope, SpendValidatingKey,
    },
    note::{
        commitment::{NoteCommitTrapdoor, NoteCommitment},
        nullifier::Nullifier,
        Note,
    },
    primitives::redpallas::{SpendAuth, VerificationKey},
    spec::NonIdentityPallasPoint,
    value::NoteValue,
};
use halo2_gadgets::{
    ecc::{
        chip::{EccChip, EccConfig},
        FixedPoint, NonIdentityPoint, Point, ScalarFixed, ScalarVar,
    },
    poseidon::{
        primitives::{self as poseidon, ConstantLength},
        Hash as PoseidonHash,
        Pow5Chip as PoseidonChip,
        Pow5Config as PoseidonConfig,
    },
    sinsemilla::chip::{SinsemillaChip, SinsemillaConfig},
    utilities::lookup_range_check::LookupRangeCheckConfig,
};

/// Public input offset for the derived nullifier.
const NF_SIGNED: usize = 0;
/// Public input offset for rk (x-coordinate).
const RK_X: usize = 1;
/// Public input offset for rk (y-coordinate).
const RK_Y: usize = 2;
/// Public input offset for the governance commitment.
const GOV_COMM: usize = 3;
/// Public input offset for the vote round identifier.
const VOTE_ROUND_ID: usize = 4;

/// Size of the delegation circuit (2^K rows).
///
/// Current usage fits within K=11 (2048 rows) but K=12 (4096 rows)
/// is chosen to leave headroom for future conditions (Merkle path,
/// SMT non-membership, governance nullifiers).
///
/// Row budget breakdown:
/// - Sinsemilla lookup table: ~1024 rows (loaded once, shared)
/// - NoteCommit (Sinsemilla hash + decomposition/canonicity + rcm scalar mul): ~950-1150 rows
/// - Nullifier derivation (Poseidon + ECC): ~400 rows
/// - Spend authority (fixed-base scalar mul + point add): ~260 rows
/// - CommitIvk (Sinsemilla short commit + decomposition): ~200 rows
/// - Address integrity (variable-base scalar mul): ~260 rows
const K: u32 = 12;

/// Configuration for the Delegation circuit.
#[derive(Clone, Debug)]
pub struct Config {
    // The instance column (public inputs)
    primary: Column<InstanceColumn>,
    // 10 advice columns for private witness data.
    // This is the scratch space where the prover places intermediate values during computation.
    // Various chips use these columns
    // Poseidon: [5..9]
    // ECC: uses all 10
    // AddChip: uses [6..9]
    advices: [Column<Advice>; 10],
    // Configuration for the AddChip which constrains a + b = c over field elements.
    // Used inside DeriveNullifier to combine intermediate values.
    add_config: AddConfig,
    // Configuration for the ECCChip which provides elliptic curve operations
    // (point addition, scalar multiplication) on the Pallas curve with Orchard's fixes bases.
    // We use it to convert cm_signed from NoteCommitment to a Field point for the DeriveNullifier function.
    ecc_config: EccConfig<OrchardFixedBases>,
    // Poseidon chip config. Used in the DeriveNullifier.
    poseidon_config: PoseidonConfig<pallas::Base, 3, 2>,
    // Sinsemilla config — used for loading the lookup table that
    // LookupRangeCheckConfig (and thus EccChip) depends on, and for CommitIvk.
    sinsemilla_config: SinsemillaConfig<OrchardHashDomains, OrchardCommitDomains, OrchardFixedBases>,
    // Configuration to handle decomposition and canonicity checking for CommitIvk.
    commit_ivk_config: CommitIvkConfig,
    // Configuration for decomposition and canonicity checking for NoteCommit.
    note_commit_config: NoteCommitConfig,
}

impl Config {
    fn add_chip(&self) -> AddChip {
        AddChip::construct(self.add_config.clone())
    }

    fn ecc_chip(&self) -> EccChip<OrchardFixedBases> {
        EccChip::construct(self.ecc_config.clone())
    }

    // Operating over the Pallas base field, with a width of 3 (state size) and rate of 2
    // 3 comes from the P128Pow5T3 construction used throughout Orchard (i.e. 3 is width)
    // Rate of 2 means that two elements are absorbed per permutation, so the hash completes
    // in fewer rounds than rate 1, roughly halving the number of Poseidon permutations.
    fn poseidon_chip(&self) -> PoseidonChip<pallas::Base, 3, 2> {
        PoseidonChip::construct(self.poseidon_config.clone())
    }

    fn commit_ivk_chip(&self) -> CommitIvkChip {
        CommitIvkChip::construct(self.commit_ivk_config.clone())
    }

    fn sinsemilla_chip(
        &self,
    ) -> SinsemillaChip<OrchardHashDomains, OrchardCommitDomains, OrchardFixedBases> {
        SinsemillaChip::construct(self.sinsemilla_config.clone())
    }

    fn note_commit_chip(&self) -> NoteCommitChip {
        NoteCommitChip::construct(self.note_commit_config.clone())
    }
}

/// The Delegation circuit.
///
/// Proves nullifier integrity, spend authority, and diversified address integrity:
/// - The prover knows `(nk, rho, psi, cm)` such that `nf_signed = DeriveNullifier(nk, rho, psi, cm)`.
/// - The prover knows `(ak, alpha)` such that `rk = [alpha] * SpendAuthG + ak`.
/// - The prover knows `(ak, nk, rivk, g_d_signed, pk_d_signed)` such that
///   `pk_d_signed = [CommitIvk_rivk(ExtractP(ak), nk)] * g_d_signed`.
#[derive(Clone, Debug, Default)]
pub struct Circuit {
    nk: Value<NullifierDerivingKey>,
    rho_signed: Value<pallas::Base>,
    psi_signed: Value<pallas::Base>,
    cm_signed: Value<NoteCommitment>,
    ak: Value<SpendValidatingKey>,
    alpha: Value<pallas::Scalar>,
    rivk: Value<CommitIvkRandomness>,
    rcm_signed: Value<NoteCommitTrapdoor>,
    g_d_signed: Value<NonIdentityPallasPoint>,
    pk_d_signed: Value<DiversifiedTransmissionKey>,
    // Rho binding witnesses (condition 3).
    cmx_1: Value<pallas::Base>,
    cmx_2: Value<pallas::Base>,
    cmx_3: Value<pallas::Base>,
    cmx_4: Value<pallas::Base>,
    gov_comm: Value<pallas::Base>,
    vote_round_id: Value<pallas::Base>,
}

impl Circuit {
    /// Constructs a `Circuit` from a note, its full viewing key, and the spend auth randomizer.
    pub fn from_note_unchecked(fvk: &FullViewingKey, note: &Note, alpha: pallas::Scalar) -> Self {
        let sender_address = note.recipient();
        let rho_signed = note.rho();
        let psi_signed = note.rseed().psi(&rho_signed);
        let rcm_signed = note.rseed().rcm(&rho_signed);
        Circuit {
            nk: Value::known(*fvk.nk()),
            rho_signed: Value::known(rho_signed.0),
            psi_signed: Value::known(psi_signed),
            cm_signed: Value::known(note.commitment()),
            ak: Value::known(fvk.clone().into()),
            alpha: Value::known(alpha),
            rivk: Value::known(fvk.rivk(Scope::External)),
            rcm_signed: Value::known(rcm_signed),
            g_d_signed: Value::known(sender_address.g_d()),
            pk_d_signed: Value::known(*sender_address.pk_d()),
            ..Default::default()
        }
    }

    /// Sets the rho-binding witness fields (condition 3).
    ///
    /// The rho of the signed note must equal
    /// `Poseidon(cmx_1, cmx_2, cmx_3, cmx_4, gov_comm, vote_round_id)`,
    /// binding the keystone signature to the exact notes being delegated,
    /// the governance commitment, and the round.
    pub fn with_rho_binding(
        mut self,
        cmx_1: pallas::Base,
        cmx_2: pallas::Base,
        cmx_3: pallas::Base,
        cmx_4: pallas::Base,
        gov_comm: pallas::Base,
        vote_round_id: pallas::Base,
    ) -> Self {
        self.cmx_1 = Value::known(cmx_1);
        self.cmx_2 = Value::known(cmx_2);
        self.cmx_3 = Value::known(cmx_3);
        self.cmx_4 = Value::known(cmx_4);
        self.gov_comm = Value::known(gov_comm);
        self.vote_round_id = Value::known(vote_round_id);
        self
    }
}

impl plonk::Circuit<pallas::Base> for Circuit {
    type Config = Config;
    type FloorPlanner = floor_planner::V1;

    fn without_witnesses(&self) -> Self {
        Self::default()
    }

    fn configure(meta: &mut plonk::ConstraintSystem<pallas::Base>) -> Self::Config {
        // Advice columns used in the circuit.
        let advices = [
            meta.advice_column(),
            meta.advice_column(),
            meta.advice_column(),
            meta.advice_column(),
            meta.advice_column(),
            meta.advice_column(),
            meta.advice_column(),
            meta.advice_column(),
            meta.advice_column(),
            meta.advice_column(),
        ];

        // Addition of two field elements.
        // 7,8,6 are chosen to share columns with the Poseidon chip to minimize column usage.
        let add_config = AddChip::configure(meta, advices[7], advices[8], advices[6]);

        // Fixed columns for the Sinsemilla generator lookup table.
        let table_idx = meta.lookup_table_column();
        let lookup = (
            table_idx,
            meta.lookup_table_column(),
            meta.lookup_table_column(),
        );

        // Instance column used for public inputs.
        let primary = meta.instance_column();
        meta.enable_equality(primary);

        // Permutation over all advice columns.
        for advice in advices.iter() {
            meta.enable_equality(*advice);
        }

        // Fixed columns shared between ECC and Poseidon chips.
        let lagrange_coeffs = [
            meta.fixed_column(),
            meta.fixed_column(),
            meta.fixed_column(),
            meta.fixed_column(),
            meta.fixed_column(),
            meta.fixed_column(),
            meta.fixed_column(),
            meta.fixed_column(),
        ];
        let rc_a = lagrange_coeffs[2..5].try_into().unwrap();
        let rc_b = lagrange_coeffs[5..8].try_into().unwrap();

        // Use the first Lagrange coefficient column for loading global constants.
        meta.enable_constant(lagrange_coeffs[0]);

        // Range check configuration using the right-most advice column.
        let range_check = LookupRangeCheckConfig::configure(meta, advices[9], table_idx);

        // Configuration for curve point operations.
        let ecc_config =
            EccChip::<OrchardFixedBases>::configure(meta, advices, lagrange_coeffs, range_check);

        // Configuration for the Poseidon hash.
        let poseidon_config = PoseidonChip::configure::<poseidon::P128Pow5T3>(
            meta,
            advices[6..9].try_into().unwrap(),
            advices[5],
            rc_a,
            rc_b,
        );

        // Sinsemilla config — used for loading the lookup table that the range check
        // (and thus ECC operations) depend on, and for CommitIvk.
        let sinsemilla_config = SinsemillaChip::configure(
            meta,
            advices[..5].try_into().unwrap(),
            advices[6],
            lagrange_coeffs[0],
            lookup,
            range_check,
        );

        // Configuration to handle decomposition and canonicity checking for CommitIvk.
        let commit_ivk_config = CommitIvkChip::configure(meta, advices);

        // Configuration for decomposition and canonicity checking for NoteCommit.
        let note_commit_config =
            NoteCommitChip::configure(meta, advices, sinsemilla_config.clone());

        Config {
            primary,
            advices,
            add_config,
            ecc_config,
            poseidon_config,
            sinsemilla_config,
            commit_ivk_config,
            note_commit_config,
        }
    }

    #[allow(non_snake_case)]
    fn synthesize(
        &self,
        config: Self::Config,
        mut layouter: impl Layouter<pallas::Base>,
    ) -> Result<(), plonk::Error> {
        // Load the Sinsemilla generator lookup table (needed by ECC range checks).
        SinsemillaChip::load(config.sinsemilla_config.clone(), &mut layouter)?;

        // Construct the ECC chip.
        // It is needed to derive cm_signed ECC point from NoteCommitment.
        let ecc_chip = config.ecc_chip();

        // Witness ak_P (spend validating key) as a non-identity curve point.
        // Shared between spend authority and CommitIvk.
        // If ak_P were allowed to be the identity point (zero of the curve group), it would be a degenerate
        // key with no cryptographic strength - any signature would trivially verify against it.
        // By constraining, we ensure that the delegated spend authority is backed by a real meaningful
        // public key.
        let ak_P: Value<pallas::Point> = self.ak.as_ref().map(|ak| ak.into());
        let ak_P = NonIdentityPoint::new(
            ecc_chip.clone(),
            layouter.namespace(|| "witness ak_P"),
            ak_P.map(|ak_P| ak_P.to_affine()),
        )?;

        // Witness g_d_signed (diversified generator from the note's address).
        // Shared between diversified address integrity check and (future) note commitment.
        let g_d_signed = NonIdentityPoint::new(
            ecc_chip.clone(),
            layouter.namespace(|| "witness g_d_signed"),
            self.g_d_signed.as_ref().map(|gd| gd.to_affine()),
        )?;

        // Witness nk (nullifier deriving key).
        let nk = assign_free_advice(
            layouter.namespace(|| "witness nk"),
            config.advices[0],
            self.nk.map(|nk| nk.inner()),
        )?;

        // Witness rho_signed.
        // This is the nullifier of the note that was spent to create this note. It is
        // a Nullifier type (a Pallas base field element) that serves as a unique, per-note domain
        // separator.
        // rho ensures that even if two notes have identical contents, they will produce
        // different nullifiers because they were created by spending different input notes.
        // rho provides deterministic, structural uniqueness. It is the nullifier of the
        // spend input note so it chains each note to its creation context. A single tx
        // can create multiple output notes from the same input. All those outputs share the same
        // rho. If nullifier derivation only used rho (no psi), outputs from the same input could collide.
        let rho_signed = assign_free_advice(
            layouter.namespace(|| "witness rho_signed"),
            config.advices[0],
            self.rho_signed,
        )?;

        // Witness psi_signed.
        // Pseudorandom field element derived from the note's random
        // seed rseed and its nullifier domain separator rho.
        // It adds randomness to the nullifier so that even if two notes share the same
        // rho and nk, they produce different nullifiers.
        // We provide it as input instead of deriving in-circuit since derivation
        // would require an expensive Blake2b.
        // psi provides randomized uniqueness. It is derived from rseed which is
        // freshly random per note. So, even if multiple outputs are derived from the same note,
        // different rseed values produce different psi values. But if uniqueness relied only on psi
        // (i.e. only randomness), a faulty RNG would cause nullifier collisions. Together with rho,
        // they cover each other's weaknesses.
        // Additionally, there is a structural reason, if we only used psi, there would be an implicit chain:
        // each note's identity is linked to the note that was spend to create it. The randomized psi
        // breaks the chain, unblocking a requirement used in Orchard's security proof.
        let psi_signed = assign_free_advice(
            layouter.namespace(|| "witness psi_signed"),
            config.advices[0],
            self.psi_signed,
        )?;

        // Witness cm_signed as an ECC point, which is the form DeriveNullifier expects.
        let cm_signed = Point::new(
            ecc_chip.clone(),
            layouter.namespace(|| "witness cm_signed"),
            self.cm_signed.as_ref().map(|cm| cm.inner().to_affine()),
        )?;

        // Nullifier integrity: derive nf_signed = DeriveNullifier(nk, rho_signed, psi_signed, cm_signed).
        let nf_signed = derive_nullifier(
            layouter.namespace(|| "nf_signed = DeriveNullifier_nk(rho_signed, psi_signed, cm_signed)"),
            config.poseidon_chip(),
            config.add_chip(),
            ecc_chip.clone(),
            rho_signed.clone(), // clone so rho_signed remains available for note_commit
            &psi_signed,
            &cm_signed,
            nk.clone(), // clone so nk remains available for commit_ivk
        )?;

        // Constrain nf_signed to equal the public input.
        // Enforce that the nullifier computed inside the circuit matches the nullifier provided
        // as a public input from outside the circuit (supplied at NF_SIGNED of the public input)
        layouter.constrain_instance(nf_signed.inner().cell(), config.primary, NF_SIGNED)?;

        // Spend authority
        // Proves that the public rk is a valid rerandomization of the prover's ak.
        // The out-of-circuit verifier checks that the keystone signature is valid under rk,
        // so this links the ZKP to the signature without revealing ak.
        {
            // Witness alpha (spend auth randomizer) as a full-width fixed scalar.
            let alpha = ScalarFixed::new(
                ecc_chip.clone(),
                layouter.namespace(|| "alpha"),
                self.alpha,
            )?;

            // alpha_commitment = [alpha] SpendAuthG
            let (alpha_commitment, _) = {
                // SpendAuthG is a fixed generator point on the Pallas elliptic curve, used
                // specifically for spend authorization in the Orchard protocol.
                let spend_auth_g = OrchardFixedBasesFull::SpendAuthG;
                let spend_auth_g = FixedPoint::from_inner(ecc_chip.clone(), spend_auth_g);
                spend_auth_g.mul(layouter.namespace(|| "[alpha] SpendAuthG"), alpha)?
            };

            // rk = [alpha] SpendAuthG + ak_P
            let rk = alpha_commitment.add(layouter.namespace(|| "rk"), &ak_P)?;

            // Constrain rk to equal the public inputs (x and y coordinates).
            layouter.constrain_instance(rk.inner().x().cell(), config.primary, RK_X)?;
            layouter.constrain_instance(rk.inner().y().cell(), config.primary, RK_Y)?;
        }

        // Diversified address integrity.
        // ivk = ⊥ or pk_d_signed = [ivk] * g_d_signed
        // where ivk = CommitIvk_rivk(ExtractP(ak_P), nk)
        //
        // The ⊥ case is handled internally by CommitDomain::short_commit:
        // incomplete addition allows ⊥ to occur, and synthesis detects
        // these edge cases and aborts proof creation.
        let pk_d_signed = {
            let ivk = {
                // ExtractP(ak_P) -- extract the x-coordinate from the curve point
                let ak = ak_P.extract_p().inner().clone();
                let rivk = ScalarFixed::new(
                    ecc_chip.clone(),
                    layouter.namespace(|| "rivk"),
                    self.rivk.map(|rivk| rivk.inner()),
                )?;

                // Commit ak and nk with rivk randomness, creating an ivk.
                commit_ivk(
                    config.sinsemilla_chip(),
                    ecc_chip.clone(),
                    config.commit_ivk_chip(),
                    layouter.namespace(|| "CommitIvk"),
                    ak,
                    nk,
                    rivk,
                )?
            };

            // Convert ivk (an x-coordinate) to a variable-base scalar for EC multiplication.
            let ivk = ScalarVar::from_base(
                ecc_chip.clone(),
                layouter.namespace(|| "ivk"),
                ivk.inner(),
            )?;

            // [ivk] g_d_signed - derive the expected pk_d
            let (derived_pk_d_signed, _ivk) =
                g_d_signed.mul(layouter.namespace(|| "[ivk] g_d_signed"), ivk)?;

            // Witness pk_d_signed and constrain it to equal the derived value.
            let pk_d_signed = NonIdentityPoint::new(
                ecc_chip.clone(),
                layouter.namespace(|| "witness pk_d_signed"),
                self.pk_d_signed.map(|pk_d_signed| pk_d_signed.inner().to_affine()),
            )?;
            derived_pk_d_signed
                .constrain_equal(layouter.namespace(|| "pk_d_signed equality"), &pk_d_signed)?;

            pk_d_signed
        };

        // signed note commitment integrity.
        // NoteCommit_rcm_signed(repr(g_d_signed), repr(pk_d_signed), 0,
        //                        rho_signed, psi_signed) = cm_signed
        // No null option: the signed note must have a valid commitment.
        {
            let rcm_signed = ScalarFixed::new(
                ecc_chip.clone(),
                layouter.namespace(|| "rcm_signed"),
                self.rcm_signed.as_ref().map(|rcm| rcm.inner()),
            )?;

            // The signed note's value is always 0.
            let v_signed = assign_free_advice(
                layouter.namespace(|| "v_signed = 0"),
                config.advices[0],
                Value::known(NoteValue::zero()),
            )?;

            // Compute NoteCommit from witness data.
            let derived_cm_signed = note_commit(
                layouter.namespace(|| "NoteCommit_rcm_signed(g_d, pk_d, 0, rho, psi)"),
                config.sinsemilla_chip(),
                config.ecc_chip(),
                config.note_commit_chip(),
                g_d_signed.inner(),
                pk_d_signed.inner(),
                v_signed,
                rho_signed.clone(),
                psi_signed,
                rcm_signed,
            )?;

            // Strict equality — no null/bottom option.
            derived_cm_signed.constrain_equal(
                layouter.namespace(|| "cm_signed integrity"),
                &cm_signed,
            )?;
        }

        // Rho binding.
        // rho_signed = Poseidon(cmx_1, cmx_2, cmx_3, cmx_4, gov_comm, vote_round_id)
        // Binds the signed note to the exact notes being delegated, the governance
        // commitment, and the round, making the keystone signature non-replayable.
        {
            let cmx_1 = assign_free_advice(
                layouter.namespace(|| "witness cmx_1"), config.advices[0], self.cmx_1)?;
            let cmx_2 = assign_free_advice(
                layouter.namespace(|| "witness cmx_2"), config.advices[0], self.cmx_2)?;
            let cmx_3 = assign_free_advice(
                layouter.namespace(|| "witness cmx_3"), config.advices[0], self.cmx_3)?;
            let cmx_4 = assign_free_advice(
                layouter.namespace(|| "witness cmx_4"), config.advices[0], self.cmx_4)?;
            let gov_comm = assign_free_advice(
                layouter.namespace(|| "witness gov_comm"), config.advices[0], self.gov_comm)?;
            let vote_round_id = assign_free_advice(
                layouter.namespace(|| "witness vote_round_id"), config.advices[0], self.vote_round_id)?;

            // Bind gov_comm and vote_round_id to the public inputs.
            layouter.constrain_instance(gov_comm.cell(), config.primary, GOV_COMM)?;
            layouter.constrain_instance(vote_round_id.cell(), config.primary, VOTE_ROUND_ID)?;

            // Poseidon hash over 6 inputs using ConstantLength<6>.
            let derived_rho = {
                let poseidon_message = [cmx_1, cmx_2, cmx_3, cmx_4, gov_comm, vote_round_id];
                let poseidon_hasher = PoseidonHash::<
                    pallas::Base, _, poseidon::P128Pow5T3, ConstantLength<6>, 3, 2,
                >::init(
                    config.poseidon_chip(),
                    layouter.namespace(|| "rho binding Poseidon init"),
                )?;
                poseidon_hasher.hash(
                    layouter.namespace(|| "Poseidon(cmx_1..4, gov_comm, vote_round_id)"),
                    poseidon_message,
                )?
            };

            // Constrain: derived_rho == rho_signed.
            layouter.assign_region(
                || "rho binding equality",
                |mut region| region.constrain_equal(derived_rho.cell(), rho_signed.cell()),
            )?;
        }

        Ok(())
    }
}

/// Public inputs to the Delegation circuit.
#[derive(Clone, Debug)]
pub struct Instance {
    /// The derived nullifier (temporary public input; will be replaced by gov_null).
    pub nf_signed: Nullifier,
    /// The randomized spend validating key, used for signature verification out-of-circuit.
    pub rk: VerificationKey<SpendAuth>,
    /// The governance commitment (public input for rho binding, condition 3).
    pub gov_comm: pallas::Base,
    /// The vote round identifier (public input for rho binding, condition 3).
    pub vote_round_id: pallas::Base,
}

impl Instance {
    /// Constructs an [`Instance`] from its constituent parts.
    pub fn from_parts(
        nf_signed: Nullifier,
        rk: VerificationKey<SpendAuth>,
        gov_comm: pallas::Base,
        vote_round_id: pallas::Base,
    ) -> Self {
        Instance {
            nf_signed,
            rk,
            gov_comm,
            vote_round_id,
        }
    }

    /// Returns the public inputs as a vector of field elements for halo2.
    pub fn to_halo2_instance(&self) -> Vec<vesta::Scalar> {
        let rk = pallas::Point::from_bytes(&self.rk.clone().into())
            .unwrap()
            .to_affine()
            .coordinates()
            .unwrap();

        vec![
            self.nf_signed.0,
            *rk.x(),
            *rk.y(),
            self.gov_comm,
            self.vote_round_id,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        keys::{FullViewingKey, Scope, SpendValidatingKey, SpendingKey},
        note::{commitment::ExtractedNoteCommitment, Note},
        spec::rho_binding_hash,
    };
    use ff::Field;
    use halo2_proofs::{dev::MockProver, plonk::Circuit as Halo2Circuit};
    use rand::rngs::OsRng;

    /// Return value from [`make_test_note`] bundling all test artefacts.
    struct TestNote {
        circuit: Circuit,
        nf: Nullifier,
        rk: VerificationKey<SpendAuth>,
        gov_comm: pallas::Base,
        vote_round_id: pallas::Base,
        cmx_1: pallas::Base,
        cmx_2: pallas::Base,
        cmx_3: pallas::Base,
        cmx_4: pallas::Base,
    }

    /// Helper: create a dummy note whose rho is derived from the rho-binding hash,
    /// along with the circuit, public inputs, and all intermediate values.
    fn make_test_note() -> TestNote {
        let mut rng = OsRng;

        // Create 4 dummy notes and extract cmx_i = ExtractP(cm_i).
        let (_, _, note1) = Note::dummy(&mut rng, None);
        let (_, _, note2) = Note::dummy(&mut rng, None);
        let (_, _, note3) = Note::dummy(&mut rng, None);
        let (_, _, note4) = Note::dummy(&mut rng, None);
        let cmx_1 = ExtractedNoteCommitment::from(note1.commitment()).inner();
        let cmx_2 = ExtractedNoteCommitment::from(note2.commitment()).inner();
        let cmx_3 = ExtractedNoteCommitment::from(note3.commitment()).inner();
        let cmx_4 = ExtractedNoteCommitment::from(note4.commitment()).inner();

        // Random governance commitment and vote round id.
        let gov_comm = pallas::Base::random(&mut rng);
        let vote_round_id = pallas::Base::random(&mut rng);

        // Derive rho from the binding hash.
        let rho = rho_binding_hash(cmx_1, cmx_2, cmx_3, cmx_4, gov_comm, vote_round_id);

        // Create the signed note with this rho.
        let sk = SpendingKey::random(&mut rng);
        let fvk: FullViewingKey = (&sk).into();
        // Re-derive with the correct FVK so nullifier/address integrity hold.
        let recipient = fvk.address_at(0u32, Scope::External);
        let note = Note::new(recipient, NoteValue::zero(), Nullifier(rho), &mut rng);

        let nf = note.nullifier(&fvk);
        let ak: SpendValidatingKey = fvk.clone().into();
        let alpha = pallas::Scalar::random(&mut rng);
        let rk = ak.randomize(&alpha);
        let circuit = Circuit::from_note_unchecked(&fvk, &note, alpha)
            .with_rho_binding(cmx_1, cmx_2, cmx_3, cmx_4, gov_comm, vote_round_id);

        TestNote { circuit, nf, rk, gov_comm, vote_round_id, cmx_1, cmx_2, cmx_3, cmx_4 }
    }

    /// Helper to build an Instance from a TestNote.
    fn make_instance(t: &TestNote) -> Instance {
        Instance::from_parts(t.nf, t.rk.clone(), t.gov_comm, t.vote_round_id)
    }

    /// Helper to build an Instance from manually constructed test data.
    /// Used by tests that bypass make_test_note() and need dummy rho-binding public inputs.
    fn make_instance_manual(
        nf: Nullifier,
        rk: VerificationKey<SpendAuth>,
    ) -> Instance {
        // These tests don't exercise rho binding, so use dummy zero values.
        // The circuit will still fail at the rho binding check since no
        // rho binding witnesses are set, but the test is targeting a different
        // constraint — the first failing constraint is what the test checks.
        Instance::from_parts(nf, rk, pallas::Base::zero(), pallas::Base::zero())
    }

    #[test]
    fn nullifier_integrity_happy_path() {
        let t = make_test_note();

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();

        let prover = MockProver::run(
            K,
            &t.circuit,
            vec![public_inputs],
        )
        .unwrap();

        assert_eq!(prover.verify(), Ok(()));
    }

    #[test]
    fn nullifier_integrity_wrong_key() {
        let mut rng = OsRng;

        // Use make_test_note for a valid circuit, then supply wrong nf.
        let t = make_test_note();

        // Derive the expected nullifier with a different key.
        let sk2 = SpendingKey::random(&mut rng);
        let fvk2: FullViewingKey = (&sk2).into();
        // We need a note to derive a wrong nf. Create a dummy one and derive nf with fvk2.
        let (_, _, dummy_note) = Note::dummy(&mut rng, None);
        let wrong_nf = dummy_note.nullifier(&fvk2);

        let instance = Instance::from_parts(wrong_nf, t.rk, t.gov_comm, t.vote_round_id);
        let public_inputs = instance.to_halo2_instance();

        let prover = MockProver::run(
            K,
            &t.circuit,
            vec![public_inputs],
        )
        .unwrap();

        // The proof should fail: the derived nullifier won't match the public input.
        assert!(prover.verify().is_err());
    }

    #[test]
    fn nullifier_integrity_dummy_note() {
        // A dummy note (value = 0) should work identically.
        let t = make_test_note();

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();

        let prover = MockProver::run(
            K,
            &t.circuit,
            vec![public_inputs],
        )
        .unwrap();

        assert_eq!(prover.verify(), Ok(()));
    }

    #[test]
    fn spend_authority_wrong_rk() {
        let mut rng = OsRng;
        let t = make_test_note();

        // Compute rk with a different alpha
        let ak: SpendValidatingKey = {
            let sk = SpendingKey::random(&mut rng);
            let fvk: FullViewingKey = (&sk).into();
            fvk.into()
        };
        let wrong_rk = ak.randomize(&pallas::Scalar::random(&mut rng));

        let instance = Instance::from_parts(t.nf, wrong_rk, t.gov_comm, t.vote_round_id);
        let public_inputs = instance.to_halo2_instance();

        let prover = MockProver::run(
            K,
            &t.circuit,
            vec![public_inputs],
        )
        .unwrap();

        // The proof should fail: the derived rk won't match the public input.
        assert!(prover.verify().is_err());
    }

    #[test]
    fn address_integrity_happy_path() {
        let t = make_test_note();

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();

        let prover = MockProver::run(K, &t.circuit, vec![public_inputs]).unwrap();
        assert_eq!(prover.verify(), Ok(()));
    }

    #[test]
    fn address_integrity_wrong_rivk() {
        let mut rng = OsRng;
        let t = make_test_note();
        let mut circuit = t.circuit.clone();

        // Replace rivk with a different key's rivk
        let sk2 = SpendingKey::random(&mut rng);
        let fvk2: FullViewingKey = (&sk2).into();
        circuit.rivk = Value::known(fvk2.rivk(Scope::External));

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    #[test]
    fn address_integrity_wrong_pk_d() {
        let mut rng = OsRng;
        let t = make_test_note();
        let mut circuit = t.circuit.clone();

        // Replace pk_d with a different key's pk_d
        let sk2 = SpendingKey::random(&mut rng);
        let fvk2: FullViewingKey = (&sk2).into();
        let other_address = fvk2.address_at(0u32, Scope::External);
        circuit.pk_d_signed = Value::known(*other_address.pk_d());

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    #[test]
    fn note_commit_integrity_happy_path() {
        let t = make_test_note();
        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &t.circuit, vec![public_inputs]).unwrap();
        assert_eq!(prover.verify(), Ok(()));
    }

    #[test]
    fn note_commit_integrity_wrong_rcm() {
        let mut rng = OsRng;
        let t = make_test_note();
        let mut circuit = t.circuit.clone();

        // Replace rcm_signed with a different note's rcm (wrong trapdoor)
        let (_sk2, _fvk2, note2) = Note::dummy(&mut rng, None);
        let rho2 = note2.rho();
        let wrong_rcm = note2.rseed().rcm(&rho2);
        circuit.rcm_signed = Value::known(wrong_rcm);

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    #[test]
    fn note_commit_integrity_wrong_cm() {
        let mut rng = OsRng;
        let t = make_test_note();
        let mut circuit = t.circuit.clone();

        let (_sk2, _fvk2, note2) = Note::dummy(&mut rng, None);
        circuit.cm_signed = Value::known(note2.commitment());

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    // ================================================================
    // Witness-tampering tests for previously uncovered private inputs.
    // Each test modifies exactly ONE witness while keeping the correct
    // public inputs, verifying that the circuit rejects the proof.
    // ================================================================

    #[test]
    fn wrong_rho_signed_witness() {
        let mut rng = OsRng;
        let t = make_test_note();
        let mut circuit = t.circuit.clone();

        // Tamper rho_signed — feeds into nullifier derivation, note commitment, AND rho binding.
        circuit.rho_signed = Value::known(pallas::Base::random(&mut rng));

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    #[test]
    fn wrong_psi_signed_witness() {
        let mut rng = OsRng;
        let t = make_test_note();
        let mut circuit = t.circuit.clone();

        // Tamper psi_signed — feeds into both nullifier derivation and note commitment.
        circuit.psi_signed = Value::known(pallas::Base::random(&mut rng));

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    #[test]
    fn wrong_g_d_signed_witness() {
        let mut rng = OsRng;
        let t = make_test_note();
        let mut circuit = t.circuit.clone();

        // Replace g_d_signed with a different key's diversified generator.
        let sk2 = SpendingKey::random(&mut rng);
        let fvk2: FullViewingKey = (&sk2).into();
        let other_address = fvk2.address_at(0u32, Scope::External);
        circuit.g_d_signed = Value::known(other_address.g_d());

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    #[test]
    fn wrong_ak_witness() {
        let mut rng = OsRng;
        let t = make_test_note();
        let mut circuit = t.circuit.clone();

        // Replace ak with a different key's ak.
        let sk2 = SpendingKey::random(&mut rng);
        let fvk2: FullViewingKey = (&sk2).into();
        circuit.ak = Value::known(fvk2.clone().into());

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    #[test]
    fn wrong_nk_witness() {
        let mut rng = OsRng;
        let t = make_test_note();
        let mut circuit = t.circuit.clone();

        // Replace nk with a different key's nk.
        let sk2 = SpendingKey::random(&mut rng);
        let fvk2: FullViewingKey = (&sk2).into();
        circuit.nk = Value::known(*fvk2.nk());

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    // ================================================================
    // Cross-constraint binding tests.
    // Verify that shared witnesses (nk, ak, rho, g_d) are actually the
    // SAME cell across constraints. An attacker who could "split" a
    // shared witness would break soundness.
    // ================================================================

    #[test]
    fn cross_constraint_ak_binds_spend_authority_and_address_integrity() {
        let mut rng = OsRng;
        let t = make_test_note();
        let mut circuit = t.circuit.clone();

        // Swap in ak from a different key — breaks spend authority
        // even if we also swap rivk/g_d/pk_d to make address integrity
        // consistent with the new ak (the circuit only has one ak cell).
        let sk2 = SpendingKey::random(&mut rng);
        let fvk2: FullViewingKey = (&sk2).into();
        let addr2 = fvk2.address_at(0u32, Scope::External);
        circuit.ak = Value::known(fvk2.clone().into());
        circuit.rivk = Value::known(fvk2.rivk(Scope::External));
        circuit.g_d_signed = Value::known(addr2.g_d());
        circuit.pk_d_signed = Value::known(*addr2.pk_d());

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    #[test]
    fn cross_constraint_rho_binds_nullifier_and_note_commit() {
        let mut rng = OsRng;
        let t = make_test_note();
        let mut circuit = t.circuit.clone();

        // Build a second note to get a different rho/commitment pair
        let (_sk2, _fvk2, note2) = Note::dummy(&mut rng, None);

        // Inject note2's rho and cm into the circuit.
        let rho2 = note2.rho();
        circuit.rho_signed = Value::known(rho2.0);
        circuit.psi_signed = Value::known(note2.rseed().psi(&rho2));
        circuit.rcm_signed = Value::known(note2.rseed().rcm(&rho2));
        circuit.cm_signed = Value::known(note2.commitment());

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    // ================================================================
    // Edge-case and structural tests.
    // ================================================================

    #[test]
    fn multiple_independent_notes_verify() {
        let t1 = make_test_note();
        let t2 = make_test_note();

        let pi1 = make_instance(&t1).to_halo2_instance();
        let pi2 = make_instance(&t2).to_halo2_instance();

        let prover1 = MockProver::run(K, &t1.circuit, vec![pi1]).unwrap();
        let prover2 = MockProver::run(K, &t2.circuit, vec![pi2]).unwrap();

        assert_eq!(prover1.verify(), Ok(()));
        assert_eq!(prover2.verify(), Ok(()));
    }

    #[test]
    fn swapped_public_inputs_fail() {
        let t1 = make_test_note();
        let t2 = make_test_note();

        // Feed t1's circuit the public inputs of t2
        let wrong_pi = make_instance(&t2).to_halo2_instance();
        let prover = MockProver::run(K, &t1.circuit, vec![wrong_pi]).unwrap();
        assert!(prover.verify().is_err());

        // And vice versa
        let wrong_pi = make_instance(&t1).to_halo2_instance();
        let prover = MockProver::run(K, &t2.circuit, vec![wrong_pi]).unwrap();
        assert!(prover.verify().is_err());
    }

    #[test]
    fn instance_to_halo2_roundtrip() {
        let t = make_test_note();
        let instance = make_instance(&t);
        let pi = instance.to_halo2_instance();

        assert_eq!(pi.len(), 5, "Expected exactly 5 public inputs (nf, rk.x, rk.y, gov_comm, vote_round_id)");
        assert_eq!(pi[NF_SIGNED], t.nf.0, "First element must be nf");

        // Reconstruct rk coordinates independently and compare.
        let rk_point = pallas::Point::from_bytes(&t.rk.into())
            .unwrap()
            .to_affine()
            .coordinates()
            .unwrap();
        assert_eq!(pi[RK_X], *rk_point.x(), "Second element must be rk.x");
        assert_eq!(pi[RK_Y], *rk_point.y(), "Third element must be rk.y");
        assert_eq!(pi[GOV_COMM], t.gov_comm, "Fourth element must be gov_comm");
        assert_eq!(pi[VOTE_ROUND_ID], t.vote_round_id, "Fifth element must be vote_round_id");
    }

    #[test]
    fn default_circuit_is_consistent_with_without_witnesses() {
        let t = make_test_note();
        let empty = Halo2Circuit::without_witnesses(&t.circuit);

        let params = halo2_proofs::poly::commitment::Params::<vesta::Affine>::new(K);
        let vk = halo2_proofs::plonk::keygen_vk(&params, &empty);
        assert!(vk.is_ok(), "keygen_vk must succeed on without_witnesses circuit");
    }

    // ================================================================
    // Rho binding tests (condition 3).
    // ================================================================

    #[test]
    fn rho_binding_happy_path() {
        let t = make_test_note();
        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();

        let prover = MockProver::run(K, &t.circuit, vec![public_inputs]).unwrap();
        assert_eq!(prover.verify(), Ok(()));
    }

    #[test]
    fn rho_binding_wrong_cmx() {
        let mut rng = OsRng;
        let t = make_test_note();
        let mut circuit = t.circuit.clone();

        // Tamper with cmx_1 in the witness. The Poseidon hash will produce
        // a different value than rho_signed, so the equality constraint fails.
        circuit.cmx_1 = Value::known(pallas::Base::random(&mut rng));

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    #[test]
    fn rho_binding_wrong_gov_comm_public_input() {
        let mut rng = OsRng;
        let t = make_test_note();

        // Supply a different gov_comm in the public instance while the circuit
        // witness has the correct gov_comm. The constrain_instance on gov_comm will fail.
        let wrong_gov_comm = pallas::Base::random(&mut rng);
        let instance = Instance::from_parts(t.nf, t.rk, wrong_gov_comm, t.vote_round_id);
        let public_inputs = instance.to_halo2_instance();

        let prover = MockProver::run(K, &t.circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    #[test]
    fn rho_binding_wrong_vote_round_id() {
        let mut rng = OsRng;
        let t = make_test_note();

        // Supply a different vote_round_id in the public instance.
        let wrong_vote_round_id = pallas::Base::random(&mut rng);
        let instance = Instance::from_parts(t.nf, t.rk, t.gov_comm, wrong_vote_round_id);
        let public_inputs = instance.to_halo2_instance();

        let prover = MockProver::run(K, &t.circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    #[test]
    fn rho_binding_wrong_cmx_4() {
        let mut rng = OsRng;
        let t = make_test_note();
        let mut circuit = t.circuit.clone();

        // Tamper with cmx_4 — the last Poseidon input. Verifies the sponge
        // doesn't silently ignore later inputs.
        circuit.cmx_4 = Value::known(pallas::Base::random(&mut rng));

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    // ================================================================
    // Witness-tampering test for alpha (spend authority private input).
    // ================================================================

    #[test]
    fn wrong_alpha_witness() {
        let mut rng = OsRng;
        let t = make_test_note();
        let mut circuit = t.circuit.clone();

        // Tamper alpha while keeping the correct rk public input.
        // The circuit computes rk' = [alpha'] * G + ak, which won't match rk.
        circuit.alpha = Value::known(pallas::Scalar::random(&mut rng));

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    // ================================================================
    // Cross-constraint: nk binds nullifier and address integrity.
    // nk is shared between DeriveNullifier (condition 2) and
    // CommitIvk (condition 5). Verify we can't split them.
    // ================================================================

    #[test]
    fn cross_constraint_nk_binds_nullifier_and_address_integrity() {
        let mut rng = OsRng;
        let t = make_test_note();
        let mut circuit = t.circuit.clone();

        // Replace nk with a different key's nk, and also adjust rivk/g_d/pk_d
        // to make CommitIvk internally consistent with the new nk.
        // If nk were split across constraints, address integrity would pass
        // while nullifier integrity would use the original nk. Since nk is a
        // single cell, both constraints see the tampered value and the proof
        // must fail (the nullifier won't match the public input).
        let sk2 = SpendingKey::random(&mut rng);
        let fvk2: FullViewingKey = (&sk2).into();
        circuit.nk = Value::known(*fvk2.nk());
        // Also swap rivk to match nk2 so CommitIvk would be internally
        // consistent if it had its own nk cell.
        circuit.rivk = Value::known(fvk2.rivk(Scope::External));

        let instance = make_instance(&t);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert!(prover.verify().is_err());
    }

    // ================================================================
    // Spec-vs-circuit consistency: rho_binding_hash.
    // ================================================================

    #[test]
    fn rho_binding_spec_matches_circuit() {
        // Independently recompute rho_binding_hash from the TestNote's cmx
        // values and verify the circuit still accepts. This guards against
        // divergence between the spec function and the in-circuit Poseidon.
        let t = make_test_note();

        // Recompute rho from the stored cmx values — independently of
        // make_test_note's own call to rho_binding_hash.
        let recomputed_rho = rho_binding_hash(
            t.cmx_1, t.cmx_2, t.cmx_3, t.cmx_4, t.gov_comm, t.vote_round_id,
        );

        // Build a fresh note with this independently derived rho.
        let mut rng = OsRng;
        let sk = SpendingKey::random(&mut rng);
        let fvk: FullViewingKey = (&sk).into();
        let recipient = fvk.address_at(0u32, Scope::External);
        let note = Note::new(recipient, NoteValue::zero(), Nullifier(recomputed_rho), &mut rng);

        let nf = note.nullifier(&fvk);
        let ak: SpendValidatingKey = fvk.clone().into();
        let alpha = pallas::Scalar::random(&mut rng);
        let rk = ak.randomize(&alpha);
        let circuit = Circuit::from_note_unchecked(&fvk, &note, alpha)
            .with_rho_binding(t.cmx_1, t.cmx_2, t.cmx_3, t.cmx_4, t.gov_comm, t.vote_round_id);

        let instance = Instance::from_parts(nf, rk, t.gov_comm, t.vote_round_id);
        let public_inputs = instance.to_halo2_instance();
        let prover = MockProver::run(K, &circuit, vec![public_inputs]).unwrap();
        assert_eq!(prover.verify(), Ok(()));
    }
}
