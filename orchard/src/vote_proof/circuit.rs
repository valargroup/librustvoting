//! The Vote Proof circuit implementation (ZKP #2).
//!
//! Proves that a registered voter is casting a valid vote, without
//! revealing which VAN they hold. Currently implements:
//!
//! - **Condition 2**: VAN Integrity (Poseidon hash).
//! - **Condition 4**: VAN Nullifier Integrity (nested Poseidon, `constrain_instance`).
//! - **Condition 5**: Proposal Authority Decrement (AddChip + range check).
//! - **Condition 6**: New VAN Integrity (Poseidon hash, `constrain_instance`).
//!
//! Remaining conditions (1, 3, 7–11) are stubbed with witness fields and
//! public input slots; constraint logic will be added incrementally.
//!
//! ## Conditions overview
//!
//! VAN ownership and spending:
//! - **Condition 1**: VAN Membership — Merkle path from `vote_authority_note_old`
//!   to `vote_comm_tree_root`.
//! - **Condition 2**: VAN Integrity — `vote_authority_note_old` is a correct
//!   Poseidon hash of its components. *(implemented)*
//! - **Condition 3**: Spend Authority — prover knows `vsk` such that
//!   `voting_hotkey_pk` is derived from `vsk`.
//! - **Condition 4**: VAN Nullifier Integrity — `van_nullifier` is correctly
//!   derived from `vsk.nk`. *(implemented)*
//!
//! New VAN construction:
//! - **Condition 5**: Proposal Authority Decrement — `proposal_authority_new =
//!   proposal_authority_old - 1`, and `proposal_authority_old > 0`. *(implemented)*
//! - **Condition 6**: New VAN Integrity — same structure as condition 2 but
//!   with decremented authority. *(implemented)*
//!
//! Vote commitment construction:
//! - **Condition 7**: Shares Sum Correctness — `sum(shares_1..4) = total_note_value`.
//! - **Condition 8**: Shares Range — each `shares_j` in `[0, 2^24)`.
//! - **Condition 9**: Shares Hash Integrity — `shares_hash = H(enc_share_1..4)`.
//! - **Condition 10**: Encryption Integrity — each `enc_share_i = ElGamal(shares_i, r_i, ea_pk)`.
//! - **Condition 11**: Vote Commitment Integrity — `vote_commitment = H(DOMAIN_VC, shares_hash,
//!   proposal_id, vote_decision)`.

use alloc::vec::Vec;

use halo2_proofs::{
    circuit::{floor_planner, AssignedCell, Layouter, Value},
    plonk::{self, Advice, Column, ConstraintSystem, Fixed, Instance as InstanceColumn, TableColumn},
};
use pasta_curves::{pallas, vesta};

use halo2_gadgets::{
    poseidon::{
        primitives::{self as poseidon, ConstantLength},
        Hash as PoseidonHash, Pow5Chip as PoseidonChip, Pow5Config as PoseidonConfig,
    },
    utilities::lookup_range_check::LookupRangeCheckConfig,
};
use crate::circuit::gadget::{
    add_chip::{AddChip, AddConfig},
    AddInstruction,
};

// ================================================================
// Constants
// ================================================================

/// Depth of the Poseidon-based vote commitment tree.
///
/// Reduced from Zcash's depth 32 (~4.3B) because governance voting
/// produces far fewer leaves than a full shielded pool. Each voter
/// generates 1 leaf per delegation + 2 per vote, so even 10K voters
/// × 50 proposals ≈ 1M leaves — well within 2^24 ≈ 16.7M capacity.
///
/// Must match `vote_commitment_tree::TREE_DEPTH`.
pub const VOTE_COMM_TREE_DEPTH: usize = 24;

/// Circuit size (2^K rows).
///
/// K=12 (4,096 rows). Conditions 2, 4, 5, 6 use ~4 Poseidon hashes
/// (~1,000 rows), plus the AddChip and range-check running sum.
/// The 10-bit lookup table requires 1,024 rows in the table column.
/// K=12 provides comfortable headroom for both.
pub const K: u32 = 12;

/// Domain tag for Vote Authority Notes.
///
/// Prepended as the first Poseidon input for domain separation from
/// Vote Commitments in the shared vote commitment tree.
/// `DOMAIN_VAN = 0` for VANs, `DOMAIN_VC = 1` for Vote Commitments.
pub const DOMAIN_VAN: u64 = 0;

// ================================================================
// Public input offsets (7 field elements).
// ================================================================

/// Public input offset for the VAN nullifier (prevents double-vote).
const VAN_NULLIFIER: usize = 0;
/// Public input offset for the new VAN commitment (with decremented authority).
const VOTE_AUTHORITY_NOTE_NEW: usize = 1;
/// Public input offset for the vote commitment hash.
const VOTE_COMMITMENT: usize = 2;
/// Public input offset for the vote commitment tree root.
const VOTE_COMM_TREE_ROOT: usize = 3;
/// Public input offset for the tree anchor height.
const VOTE_COMM_TREE_ANCHOR_HEIGHT: usize = 4;
/// Public input offset for the proposal identifier.
const PROPOSAL_ID: usize = 5;
/// Public input offset for the voting round identifier.
const VOTING_ROUND_ID: usize = 6;

// Suppress dead-code warnings for public input offsets that are
// defined but not yet used by any condition's constraint logic.
// These will be used as conditions 1, 3, 7–11 are implemented.
const _: usize = VOTE_COMMITMENT;
const _: usize = VOTE_COMM_TREE_ROOT;
const _: usize = VOTE_COMM_TREE_ANCHOR_HEIGHT;
const _: usize = PROPOSAL_ID;

// ================================================================
// Out-of-circuit helpers
// ================================================================

/// Out-of-circuit VAN integrity hash (condition 2).
///
/// Computes:
/// ```text
/// Poseidon(DOMAIN_VAN, voting_hotkey_pk, total_note_value,
///          voting_round_id, proposal_authority_old, gov_comm_rand)
/// ```
///
/// Used by the builder and tests to compute the expected VAN commitment.
pub fn van_integrity_hash(
    voting_hotkey_pk: pallas::Base,
    total_note_value: pallas::Base,
    voting_round_id: pallas::Base,
    proposal_authority_old: pallas::Base,
    gov_comm_rand: pallas::Base,
) -> pallas::Base {
    poseidon::Hash::<_, poseidon::P128Pow5T3, ConstantLength<6>, 3, 2>::init().hash([
        pallas::Base::from(DOMAIN_VAN),
        voting_hotkey_pk,
        total_note_value,
        voting_round_id,
        proposal_authority_old,
        gov_comm_rand,
    ])
}

/// Returns the domain separator for the VAN nullifier inner hash.
///
/// Encodes `"vote authority spend"` as a Pallas base field element
/// by interpreting the UTF-8 bytes as a little-endian 256-bit integer.
/// This domain tag differentiates VAN nullifier derivation from other
/// Poseidon uses in the protocol.
pub fn domain_van_nullifier() -> pallas::Base {
    // "vote authority spend" (20 bytes) zero-padded to 32, as LE u64 words.
    pallas::Base::from_raw([
        0x7475_6120_6574_6f76, // b"vote aut" LE
        0x7320_7974_6972_6f68, // b"hority s" LE
        0x0000_0000_646e_6570, // b"pend\0\0\0\0" LE
        0,
    ])
}

/// Out-of-circuit VAN nullifier hash (condition 4).
///
/// Three-layer `ConstantLength<2>` Poseidon chain (matches ZKP 1
/// condition 14's governance nullifier pattern):
/// ```text
/// step1  = Poseidon(voting_round_id, vote_authority_note_old)   // scope to round + VAN
/// step2  = Poseidon("vote authority spend", step1)              // domain separation
/// van_nullifier = Poseidon(vsk_nk, step2)                       // key with nk
/// ```
///
/// Used by the builder and tests to compute the expected VAN nullifier.
pub fn van_nullifier_hash(
    vsk_nk: pallas::Base,
    voting_round_id: pallas::Base,
    vote_authority_note_old: pallas::Base,
) -> pallas::Base {
    // Step 1: Poseidon(voting_round_id, vote_authority_note_old) — scope to round + VAN.
    let step1 =
        poseidon::Hash::<_, poseidon::P128Pow5T3, ConstantLength<2>, 3, 2>::init().hash([
            voting_round_id,
            vote_authority_note_old,
        ]);
    // Step 2: Poseidon(domain_tag, step1) — domain separation.
    let step2 =
        poseidon::Hash::<_, poseidon::P128Pow5T3, ConstantLength<2>, 3, 2>::init().hash([
            domain_van_nullifier(),
            step1,
        ]);
    // Step 3: Poseidon(vsk_nk, step2) — key the result so it can't be reversed.
    poseidon::Hash::<_, poseidon::P128Pow5T3, ConstantLength<2>, 3, 2>::init()
        .hash([vsk_nk, step2])
}

// ================================================================
// Config
// ================================================================

/// Configuration for the Vote Proof circuit.
///
/// Holds chip configs for Poseidon (conditions 2, 4, 6), AddChip
/// (condition 5), and LookupRangeCheck (condition 5). Will be
/// extended with ECC and custom gates as conditions 1, 3, 7–11 are
/// added.
#[derive(Clone, Debug)]
pub struct Config {
    /// Public input column (7 field elements).
    primary: Column<InstanceColumn>,
    /// 10 advice columns for private witness data.
    ///
    /// Column layout follows the delegation circuit for consistency:
    /// - `advices[0..5]`: general witness assignment (future: Sinsemilla/Merkle).
    /// - `advices[5]`: Poseidon partial S-box column.
    /// - `advices[6..9]`: Poseidon state columns + AddChip output.
    /// - `advices[9]`: range check running sum.
    advices: [Column<Advice>; 10],
    /// Poseidon hash chip configuration.
    ///
    /// P128Pow5T3 with width 3, rate 2. Used for VAN integrity (condition 2),
    /// VAN nullifier (condition 4), new VAN integrity (condition 6),
    /// vote commitment Merkle path (condition 1), and vote commitment
    /// integrity (conditions 9, 11).
    poseidon_config: PoseidonConfig<pallas::Base, 3, 2>,
    /// AddChip: constrains `a + b = c` on a single row.
    ///
    /// Uses advices[7] (a), advices[8] (b), advices[6] (c), matching
    /// the delegation circuit's column assignment.
    /// Used in condition 5 (proposal authority decrement).
    add_config: AddConfig,
    /// 10-bit lookup range check configuration.
    ///
    /// Uses advices[9] as the running-sum column. Each word is 10 bits,
    /// so `num_words` × 10 gives the total bit-width checked.
    /// Used in condition 5 to ensure `proposal_authority_old > 0`.
    range_check: LookupRangeCheckConfig<pallas::Base, 10>,
    /// Lookup table column for the 10-bit range check.
    ///
    /// Populated with [0, 2^10) during synthesis. Stored here because
    /// the vote proof circuit doesn't use Sinsemilla (which would
    /// normally load this table as a side effect).
    table_idx: TableColumn,
}

impl Config {
    /// Constructs a Poseidon chip from this configuration.
    ///
    /// Width 3 (P128Pow5T3 state size), rate 2 (absorbs 2 field elements
    /// per permutation — halves the number of rounds vs rate 1).
    pub(crate) fn poseidon_chip(&self) -> PoseidonChip<pallas::Base, 3, 2> {
        PoseidonChip::construct(self.poseidon_config.clone())
    }

    /// Constructs an AddChip for field element addition (`c = a + b`).
    fn add_chip(&self) -> AddChip {
        AddChip::construct(self.add_config.clone())
    }

    /// Returns the range check configuration (10-bit words).
    fn range_check_config(&self) -> LookupRangeCheckConfig<pallas::Base, 10> {
        self.range_check
    }
}

// ================================================================
// Circuit
// ================================================================

/// The Vote Proof circuit (ZKP #2).
///
/// Proves that a registered voter is casting a valid vote, without
/// revealing which VAN they hold. Contains witness fields for all
/// 11 conditions; constraint logic is added incrementally.
///
/// Currently constrained: conditions 2, 4, 5, 6 (VAN integrity,
/// nullifier, authority decrement, new VAN integrity).
#[derive(Clone, Debug, Default)]
pub struct Circuit {
    // === VAN ownership and spending (conditions 1–4) ===

    // Condition 1 (VAN Membership): Poseidon-based Merkle path from
    // vote_authority_note_old to vote_comm_tree_root.
    /// Merkle authentication path (sibling hashes at each tree level).
    pub(crate) vote_comm_tree_path: Value<[pallas::Base; VOTE_COMM_TREE_DEPTH]>,
    /// Leaf position in the vote commitment tree.
    pub(crate) vote_comm_tree_position: Value<u32>,

    // Condition 2 (VAN Integrity): Poseidon(DOMAIN_VAN, voting_hotkey_pk,
    // total_note_value, voting_round_id, proposal_authority_old, gov_comm_rand).
    /// The voting hotkey public key (x-coordinate of the ECC point derived from vsk).
    pub(crate) voting_hotkey_pk: Value<pallas::Base>,
    /// The voter's total delegated weight.
    pub(crate) total_note_value: Value<pallas::Base>,
    /// Remaining proposal authority bitmask in the old VAN.
    pub(crate) proposal_authority_old: Value<pallas::Base>,
    /// Blinding randomness for the VAN commitment.
    pub(crate) gov_comm_rand: Value<pallas::Base>,
    /// The old VAN commitment (Poseidon hash output). Used as the Merkle
    /// leaf in condition 1 and constrained to equal the derived hash here.
    pub(crate) vote_authority_note_old: Value<pallas::Base>,

    // Condition 3 (Spend Authority): prover knows vsk such that
    // voting_hotkey_pk is correctly derived from vsk.
    /// Voting spending key (scalar for ECC multiplication).
    pub(crate) vsk: Value<pallas::Scalar>,

    // Condition 4 (VAN Nullifier Integrity): nullifier deriving key.
    /// Nullifier deriving key derived from vsk.
    pub(crate) vsk_nk: Value<pallas::Base>,

    // === Vote commitment construction (conditions 7–11) ===

    // Condition 7 (Shares Sum): sum(shares_1..4) = total_note_value.
    // Condition 8 (Shares Range): each share in [0, 2^24).
    /// Voting share vector (4 shares that sum to total_note_value).
    pub(crate) shares: [Value<pallas::Base>; 4],

    // Condition 10 (Encryption Integrity): El Gamal randomness per share.
    /// El Gamal encryption randomness for each share.
    pub(crate) share_randomness: [Value<pallas::Scalar>; 4],

    // Condition 11 (Vote Commitment Integrity): vote decision.
    /// The voter's choice (hidden inside the vote commitment).
    pub(crate) vote_decision: Value<pallas::Base>,
}

impl Circuit {
    /// Creates a circuit with conditions 2, 4, 5, and 6 witnesses populated.
    ///
    /// All other witness fields are set to `Value::unknown()`.
    /// Conditions 4, 5, and 6 share witness cells with condition 2:
    /// - Condition 4 reuses `vote_authority_note_old` and `voting_round_id`.
    /// - Condition 5 derives `proposal_authority_new` from
    ///   `proposal_authority_old`.
    /// - Condition 6 reuses all condition 2 witnesses except
    ///   `proposal_authority_old`, which is replaced by the
    ///   in-circuit `proposal_authority_new` from condition 5.
    pub fn with_van_integrity_witnesses(
        voting_hotkey_pk: Value<pallas::Base>,
        total_note_value: Value<pallas::Base>,
        proposal_authority_old: Value<pallas::Base>,
        gov_comm_rand: Value<pallas::Base>,
        vote_authority_note_old: Value<pallas::Base>,
        vsk_nk: Value<pallas::Base>,
    ) -> Self {
        Circuit {
            voting_hotkey_pk,
            total_note_value,
            proposal_authority_old,
            gov_comm_rand,
            vote_authority_note_old,
            vsk_nk,
            ..Default::default()
        }
    }
}

/// Loads a private witness value into a fresh advice cell.
///
/// Each call gets its own single-row region, matching the delegation
/// circuit's `assign_free_advice` helper pattern.
fn assign_free_advice(
    mut layouter: impl Layouter<pallas::Base>,
    column: Column<Advice>,
    value: Value<pallas::Base>,
) -> Result<AssignedCell<pallas::Base, pallas::Base>, plonk::Error> {
    layouter.assign_region(
        || "load private",
        |mut region| region.assign_advice(|| "private input", column, 0, || value),
    )
}

impl plonk::Circuit<pallas::Base> for Circuit {
    type Config = Config;
    type FloorPlanner = floor_planner::V1;

    fn without_witnesses(&self) -> Self {
        Self::default()
    }

    fn configure(meta: &mut ConstraintSystem<pallas::Base>) -> Self::Config {
        // 10 advice columns, matching delegation circuit layout.
        let advices: [Column<Advice>; 10] = core::array::from_fn(|_| meta.advice_column());
        for col in &advices {
            meta.enable_equality(*col);
        }

        // Instance column for public inputs.
        let primary = meta.instance_column();
        meta.enable_equality(primary);

        // 8 fixed columns shared between ECC and Poseidon chips.
        // Indices 0–1: Lagrange coefficients / constants.
        // Indices 2–4: Poseidon round constants A (rc_a).
        // Indices 5–7: Poseidon round constants B (rc_b).
        let lagrange_coeffs: [Column<Fixed>; 8] =
            core::array::from_fn(|_| meta.fixed_column());
        let rc_a = lagrange_coeffs[2..5].try_into().unwrap();
        let rc_b = lagrange_coeffs[5..8].try_into().unwrap();

        // Enable constants via the first fixed column (for DOMAIN_VAN,
        // ONE, and future fixed values).
        meta.enable_constant(lagrange_coeffs[0]);

        // AddChip: constrains `a + b = c` in a single row.
        // Column assignment matches the delegation circuit:
        //   a = advices[7], b = advices[8], c = advices[6].
        let add_config = AddChip::configure(meta, advices[7], advices[8], advices[6]);

        // Lookup table column for 10-bit range checks.
        // Populated with [0, 2^10) during synthesis.
        let table_idx = meta.lookup_table_column();

        // Range check configuration: 10-bit lookup words in advices[9].
        let range_check = LookupRangeCheckConfig::configure(meta, advices[9], table_idx);

        // Poseidon chip: P128Pow5T3 with width 3, rate 2.
        // State columns: advices[6..9] (3 columns for the width-3 state).
        // Partial S-box column: advices[5].
        // Round constants: lagrange_coeffs[2..5] (rc_a), [5..8] (rc_b).
        let poseidon_config = PoseidonChip::configure::<poseidon::P128Pow5T3>(
            meta,
            advices[6..9].try_into().unwrap(),
            advices[5],
            rc_a,
            rc_b,
        );

        Config {
            primary,
            advices,
            poseidon_config,
            add_config,
            range_check,
            table_idx,
        }
    }

    #[allow(non_snake_case)]
    fn synthesize(
        &self,
        config: Self::Config,
        mut layouter: impl Layouter<pallas::Base>,
    ) -> Result<(), plonk::Error> {
        // ---------------------------------------------------------------
        // Load the 10-bit lookup table for range checks (condition 5).
        //
        // Populates [0, 2^10) into the table column. In the delegation
        // circuit this is loaded as a side effect of SinsemillaChip::load;
        // the vote proof circuit doesn't use Sinsemilla, so we load it
        // directly.
        // ---------------------------------------------------------------
        layouter.assign_table(
            || "table_idx",
            |mut table| {
                for index in 0..(1 << 10) {
                    table.assign_cell(
                        || "table_idx",
                        config.table_idx,
                        index,
                        || Value::known(pallas::Base::from(index as u64)),
                    )?;
                }
                Ok(())
            },
        )?;

        // ---------------------------------------------------------------
        // Witness assignment for condition 2.
        // ---------------------------------------------------------------

        // Copy voting_round_id from the instance column into an advice cell.
        // This creates an equality constraint between the advice cell and the
        // instance at offset VOTING_ROUND_ID, ensuring the in-circuit value
        // matches the public input.
        let voting_round_id = layouter.assign_region(
            || "copy voting_round_id from instance",
            |mut region| {
                region.assign_advice_from_instance(
                    || "voting_round_id",
                    config.primary,
                    VOTING_ROUND_ID,
                    config.advices[0],
                    0,
                )
            },
        )?;

        // Private witnesses for condition 2.
        let voting_hotkey_pk = assign_free_advice(
            layouter.namespace(|| "witness voting_hotkey_pk"),
            config.advices[0],
            self.voting_hotkey_pk,
        )?;

        let total_note_value = assign_free_advice(
            layouter.namespace(|| "witness total_note_value"),
            config.advices[0],
            self.total_note_value,
        )?;

        let proposal_authority_old = assign_free_advice(
            layouter.namespace(|| "witness proposal_authority_old"),
            config.advices[0],
            self.proposal_authority_old,
        )?;

        let gov_comm_rand = assign_free_advice(
            layouter.namespace(|| "witness gov_comm_rand"),
            config.advices[0],
            self.gov_comm_rand,
        )?;

        let vote_authority_note_old = assign_free_advice(
            layouter.namespace(|| "witness vote_authority_note_old"),
            config.advices[0],
            self.vote_authority_note_old,
        )?;

        // DOMAIN_VAN — constant-constrained so the value is baked into the
        // verification key and cannot be altered by a malicious prover.
        let domain_van = layouter.assign_region(
            || "DOMAIN_VAN constant",
            |mut region| {
                region.assign_advice_from_constant(
                    || "domain_van",
                    config.advices[0],
                    0,
                    pallas::Base::from(DOMAIN_VAN),
                )
            },
        )?;

        // Clone cells that are consumed by condition 2's Poseidon hash but
        // reused in later conditions:
        // - voting_round_id: also used in condition 4 (VAN nullifier).
        // - voting_hotkey_pk, total_note_value, voting_round_id,
        //   proposal_authority_old, gov_comm_rand, domain_van: also used
        //   in condition 6 (new VAN integrity).
        let voting_round_id_cond4 = voting_round_id.clone();
        let domain_van_cond6 = domain_van.clone();
        let voting_hotkey_pk_cond6 = voting_hotkey_pk.clone();
        let total_note_value_cond6 = total_note_value.clone();
        let voting_round_id_cond6 = voting_round_id.clone();
        let proposal_authority_old_cond5 = proposal_authority_old.clone();
        let gov_comm_rand_cond6 = gov_comm_rand.clone();

        // ---------------------------------------------------------------
        // Condition 2: VAN Integrity.
        // vote_authority_note_old = Poseidon(DOMAIN_VAN, voting_hotkey_pk,
        //     total_note_value, voting_round_id, proposal_authority_old,
        //     gov_comm_rand)
        // ---------------------------------------------------------------

        let derived_van = {
            let message = [
                domain_van,
                voting_hotkey_pk,
                total_note_value,
                voting_round_id,
                proposal_authority_old,
                gov_comm_rand,
            ];
            // Width 3 and rate 2: P128Pow5T3 state size (3 field elements) and sponge rate.
            // Rate 2 absorbs 2 elements per permutation -> 3 rounds for 6 inputs; fewer constraints than rate 1.
            let poseidon_hasher = PoseidonHash::<
                pallas::Base,
                _,
                poseidon::P128Pow5T3,
                ConstantLength<6>,
                3, // WIDTH (state size, from P128Pow5T3)
                2, // RATE (elements absorbed per permutation)
            >::init(
                config.poseidon_chip(),
                layouter.namespace(|| "VAN integrity Poseidon init"),
            )?;
            poseidon_hasher.hash(
                layouter.namespace(|| {
                    "Poseidon(DOMAIN_VAN, voting_hotkey_pk, total_note_value, \
                     voting_round_id, proposal_authority_old, gov_comm_rand)"
                }),
                message,
            )?
        };

        // Constrain: derived VAN hash == witnessed vote_authority_note_old.
        layouter.assign_region(
            || "VAN integrity check",
            |mut region| region.constrain_equal(derived_van.cell(), vote_authority_note_old.cell()),
        )?;

        // ---------------------------------------------------------------
        // Witness assignment for condition 4.
        // ---------------------------------------------------------------

        // Private witness: nullifier deriving key.
        let vsk_nk = assign_free_advice(
            layouter.namespace(|| "witness vsk_nk"),
            config.advices[0],
            self.vsk_nk,
        )?;

        // "vote authority spend" domain tag — constant-constrained so the
        // value is baked into the verification key.
        let domain_van_nf = layouter.assign_region(
            || "DOMAIN_VAN_NULLIFIER constant",
            |mut region| {
                region.assign_advice_from_constant(
                    || "domain_van_nullifier",
                    config.advices[0],
                    0,
                    domain_van_nullifier(),
                )
            },
        )?;

        // ---------------------------------------------------------------
        // Condition 4: VAN Nullifier Integrity.
        // van_nullifier = Poseidon(vsk_nk,
        //     Poseidon(domain, Poseidon(voting_round_id, vote_authority_note_old)))
        //
        // Three-layer ConstantLength<2> chain matching ZKP 1 condition 14:
        //   Step 1: scope to round + VAN
        //   Step 2: domain separation
        //   Step 3: key with nullifier key
        //
        // voting_round_id and vote_authority_note_old are reused from
        // condition 2 via cell equality — these cells flow directly into
        // the Poseidon state without being re-witnessed.
        // ---------------------------------------------------------------

        let van_nullifier = {
            // Step 1: Poseidon(voting_round_id, vote_authority_note_old)
            // — scope to this round + VAN.
            let step1_hasher = PoseidonHash::<
                pallas::Base,
                _,
                poseidon::P128Pow5T3,
                ConstantLength<2>,
                3, // WIDTH
                2, // RATE
            >::init(
                config.poseidon_chip(),
                layouter.namespace(|| "VAN nullifier step 1 init"),
            )?;
            let step1 = step1_hasher.hash(
                layouter.namespace(|| "Poseidon(voting_round_id, vote_authority_note_old)"),
                [voting_round_id_cond4, vote_authority_note_old],
            )?;

            // Step 2: Poseidon(domain_tag, step1) — domain separation.
            let step2_hasher = PoseidonHash::<
                pallas::Base,
                _,
                poseidon::P128Pow5T3,
                ConstantLength<2>,
                3, // WIDTH
                2, // RATE
            >::init(
                config.poseidon_chip(),
                layouter.namespace(|| "VAN nullifier step 2 init"),
            )?;
            let step2 = step2_hasher.hash(
                layouter.namespace(|| "Poseidon(domain_van_nullifier, step1)"),
                [domain_van_nf, step1],
            )?;

            // Step 3: Poseidon(vsk_nk, step2) — key the result so it
            // can't be reversed without knowing the spending key.
            let step3_hasher = PoseidonHash::<
                pallas::Base,
                _,
                poseidon::P128Pow5T3,
                ConstantLength<2>,
                3, // WIDTH
                2, // RATE
            >::init(
                config.poseidon_chip(),
                layouter.namespace(|| "VAN nullifier step 3 init"),
            )?;
            step3_hasher.hash(
                layouter.namespace(|| "Poseidon(vsk_nk, step2)"),
                [vsk_nk, step2],
            )?
        };

        // Bind the derived nullifier to the VAN_NULLIFIER public input.
        // The verifier checks that the prover's computed nullifier matches
        // the publicly posted value, preventing double-voting.
        layouter.constrain_instance(van_nullifier.cell(), config.primary, VAN_NULLIFIER)?;

        // ---------------------------------------------------------------
        // Condition 5: Proposal Authority Decrement.
        //
        // proposal_authority_new = proposal_authority_old - 1
        // proposal_authority_old > 0
        //
        // Proved by witnessing `diff = proposal_authority_old - 1`,
        // constraining `diff + 1 == proposal_authority_old` via AddChip,
        // and range-checking `diff` to [0, 2^70).
        //
        // If proposal_authority_old == 0, then diff wraps to p - 1
        // (≈ 2^254), which fails the 70-bit range check. This enforces
        // proposal_authority_old > 0.
        //
        // 70 bits (7 × 10-bit words) is generous for a proposal authority
        // counter; it matches the delegation circuit's range-check pattern.
        // ---------------------------------------------------------------

        let proposal_authority_new = {
            // Witness diff = proposal_authority_old - 1.
            let diff = self.proposal_authority_old
                .map(|v| v - pallas::Base::one());
            let diff = assign_free_advice(
                layouter.namespace(|| "witness proposal_authority_new"),
                config.advices[0],
                diff,
            )?;

            // Assign 1 as a constant-constrained advice cell.
            // Baked into the verification key so the decrement amount
            // cannot be changed by a malicious prover.
            let one = layouter.assign_region(
                || "ONE constant (condition 5)",
                |mut region| {
                    region.assign_advice_from_constant(
                        || "one",
                        config.advices[0],
                        0,
                        pallas::Base::one(),
                    )
                },
            )?;

            // Constrain: diff + 1 == proposal_authority_old.
            // This proves proposal_authority_new + 1 == proposal_authority_old,
            // i.e., proposal_authority_new = proposal_authority_old - 1.
            let recomputed = config.add_chip().add(
                layouter.namespace(|| "proposal_authority_new + 1"),
                &diff,
                &one,
            )?;
            layouter.assign_region(
                || "proposal_authority_old = proposal_authority_new + 1",
                |mut region| {
                    region.constrain_equal(
                        recomputed.cell(),
                        proposal_authority_old_cond5.cell(),
                    )
                },
            )?;

            // Range-check diff to [0, 2^70).
            // 7 words × 10 bits = 70 bits.
            // If proposal_authority_old == 0, diff wraps to p - 1 ≈ 2^254,
            // which fails this check — enforcing proposal_authority_old > 0.
            let proposal_authority_new = diff.clone();
            config.range_check_config().copy_check(
                layouter.namespace(|| "proposal_authority_new < 2^70"),
                diff,
                7,    // num_words: 7 × 10 = 70 bits
                true, // strict: running sum terminates at 0
            )?;

            proposal_authority_new
        };

        // ---------------------------------------------------------------
        // Condition 6: New VAN Integrity.
        //
        // vote_authority_note_new = Poseidon(DOMAIN_VAN, voting_hotkey_pk,
        //     total_note_value, voting_round_id, proposal_authority_new,
        //     gov_comm_rand)
        //
        // Structurally identical to condition 2, with
        // proposal_authority_new (from condition 5) replacing
        // proposal_authority_old. All other inputs are cell-equality-
        // linked to the same witness cells used in condition 2.
        // ---------------------------------------------------------------

        let derived_van_new = {
            let message = [
                domain_van_cond6,
                voting_hotkey_pk_cond6,
                total_note_value_cond6,
                voting_round_id_cond6,
                proposal_authority_new,
                gov_comm_rand_cond6,
            ];
            let poseidon_hasher = PoseidonHash::<
                pallas::Base,
                _,
                poseidon::P128Pow5T3,
                ConstantLength<6>,
                3, // WIDTH
                2, // RATE
            >::init(
                config.poseidon_chip(),
                layouter.namespace(|| "New VAN integrity Poseidon init"),
            )?;
            poseidon_hasher.hash(
                layouter.namespace(|| {
                    "Poseidon(DOMAIN_VAN, voting_hotkey_pk, total_note_value, \
                     voting_round_id, proposal_authority_new, gov_comm_rand)"
                }),
                message,
            )?
        };

        // Bind the derived new VAN to the VOTE_AUTHORITY_NOTE_NEW public input.
        // The verifier checks that the new VAN commitment posted on-chain is
        // correctly formed with decremented proposal authority.
        layouter.constrain_instance(
            derived_van_new.cell(),
            config.primary,
            VOTE_AUTHORITY_NOTE_NEW,
        )?;

        Ok(())
    }
}

// ================================================================
// Instance (public inputs)
// ================================================================

/// Public inputs to the Vote Proof circuit (7 field elements).
///
/// These are the values posted to the vote chain that both the prover
/// and verifier agree on. The verifier checks the proof against these
/// values without seeing any private witnesses.
#[derive(Clone, Debug)]
pub struct Instance {
    /// The nullifier of the old VAN being spent (prevents double-vote).
    pub van_nullifier: pallas::Base,
    /// The new VAN commitment (with decremented proposal authority).
    pub vote_authority_note_new: pallas::Base,
    /// The vote commitment hash.
    pub vote_commitment: pallas::Base,
    /// Root of the vote commitment tree at anchor height.
    pub vote_comm_tree_root: pallas::Base,
    /// The vote-chain height at which the tree is snapshotted.
    pub vote_comm_tree_anchor_height: pallas::Base,
    /// Which proposal this vote is for.
    pub proposal_id: pallas::Base,
    /// The voting round identifier.
    pub voting_round_id: pallas::Base,
}

impl Instance {
    /// Constructs an [`Instance`] from its constituent parts.
    pub fn from_parts(
        van_nullifier: pallas::Base,
        vote_authority_note_new: pallas::Base,
        vote_commitment: pallas::Base,
        vote_comm_tree_root: pallas::Base,
        vote_comm_tree_anchor_height: pallas::Base,
        proposal_id: pallas::Base,
        voting_round_id: pallas::Base,
    ) -> Self {
        Instance {
            van_nullifier,
            vote_authority_note_new,
            vote_commitment,
            vote_comm_tree_root,
            vote_comm_tree_anchor_height,
            proposal_id,
            voting_round_id,
        }
    }

    /// Serializes public inputs for halo2 proof creation/verification.
    ///
    /// The order must match the instance column offsets defined at the
    /// top of this file (`VAN_NULLIFIER`, `VOTE_AUTHORITY_NOTE_NEW`, etc.).
    pub fn to_halo2_instance(&self) -> Vec<vesta::Scalar> {
        alloc::vec![
            self.van_nullifier,
            self.vote_authority_note_new,
            self.vote_commitment,
            self.vote_comm_tree_root,
            self.vote_comm_tree_anchor_height,
            self.proposal_id,
            self.voting_round_id,
        ]
    }
}

// ================================================================
// Tests
// ================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use ff::Field;
    use halo2_proofs::dev::MockProver;
    use pasta_curves::pallas;
    use rand::rngs::OsRng;

    /// Build valid test data for conditions 2, 4, 5, and 6.
    ///
    /// Returns a circuit with correctly-hashed VAN witnesses and a
    /// matching instance. `proposal_authority_old` is set to a small
    /// positive value (5) so conditions 5 and 6 can decrement it.
    /// Conditions not yet constrained use placeholder values (zero).
    fn make_van_integrity_test_data() -> (Circuit, Instance) {
        let mut rng = OsRng;

        // Random witness values.
        let voting_hotkey_pk = pallas::Base::random(&mut rng);
        let total_note_value = pallas::Base::random(&mut rng);
        let voting_round_id = pallas::Base::random(&mut rng);
        // Use a known small positive value so condition 5 can decrement.
        let proposal_authority_old = pallas::Base::from(5u64);
        let gov_comm_rand = pallas::Base::random(&mut rng);
        let vsk_nk = pallas::Base::random(&mut rng);

        // Compute expected VAN commitment out-of-circuit (condition 2).
        let vote_authority_note_old = van_integrity_hash(
            voting_hotkey_pk,
            total_note_value,
            voting_round_id,
            proposal_authority_old,
            gov_comm_rand,
        );

        // Compute expected VAN nullifier out-of-circuit (condition 4).
        let van_nullifier = van_nullifier_hash(vsk_nk, voting_round_id, vote_authority_note_old);

        // Compute expected new VAN commitment out-of-circuit (condition 6).
        // Same hash as condition 2 but with proposal_authority_old - 1.
        let proposal_authority_new = proposal_authority_old - pallas::Base::one();
        let vote_authority_note_new = van_integrity_hash(
            voting_hotkey_pk,
            total_note_value,
            voting_round_id,
            proposal_authority_new,
            gov_comm_rand,
        );

        let circuit = Circuit::with_van_integrity_witnesses(
            Value::known(voting_hotkey_pk),
            Value::known(total_note_value),
            Value::known(proposal_authority_old),
            Value::known(gov_comm_rand),
            Value::known(vote_authority_note_old),
            Value::known(vsk_nk),
        );

        // Conditions 2, 4, 5, 6 constrain van_nullifier,
        // vote_authority_note_new, and voting_round_id.
        // Other public inputs use placeholder values (zero).
        let instance = Instance::from_parts(
            van_nullifier,
            vote_authority_note_new, // condition 6
            pallas::Base::zero(),    // vote_commitment (condition 11)
            pallas::Base::zero(),    // vote_comm_tree_root (condition 1)
            pallas::Base::zero(),    // vote_comm_tree_anchor_height
            pallas::Base::zero(),    // proposal_id (condition 11)
            voting_round_id,
        );

        (circuit, instance)
    }

    // ================================================================
    // Condition 2 (VAN Integrity) tests
    // ================================================================

    #[test]
    fn van_integrity_valid_proof() {
        let (circuit, instance) = make_van_integrity_test_data();

        let prover = MockProver::run(K, &circuit, vec![instance.to_halo2_instance()]).unwrap();

        assert_eq!(prover.verify(), Ok(()));
    }

    #[test]
    fn van_integrity_wrong_hash_fails() {
        let mut rng = OsRng;

        let voting_hotkey_pk = pallas::Base::random(&mut rng);
        let total_note_value = pallas::Base::random(&mut rng);
        let voting_round_id = pallas::Base::random(&mut rng);
        let proposal_authority_old = pallas::Base::from(5u64);
        let gov_comm_rand = pallas::Base::random(&mut rng);
        let vsk_nk = pallas::Base::random(&mut rng);

        // Deliberately wrong VAN value — does not match the Poseidon hash.
        let wrong_van = pallas::Base::random(&mut rng);

        let circuit = Circuit::with_van_integrity_witnesses(
            Value::known(voting_hotkey_pk),
            Value::known(total_note_value),
            Value::known(proposal_authority_old),
            Value::known(gov_comm_rand),
            Value::known(wrong_van),
            Value::known(vsk_nk),
        );

        // Compute correct values for the instance.
        let correct_van = van_integrity_hash(
            voting_hotkey_pk,
            total_note_value,
            voting_round_id,
            proposal_authority_old,
            gov_comm_rand,
        );
        let van_nullifier = van_nullifier_hash(vsk_nk, voting_round_id, correct_van);
        let proposal_authority_new = proposal_authority_old - pallas::Base::one();
        let vote_authority_note_new = van_integrity_hash(
            voting_hotkey_pk,
            total_note_value,
            voting_round_id,
            proposal_authority_new,
            gov_comm_rand,
        );

        let instance = Instance::from_parts(
            van_nullifier,
            vote_authority_note_new,
            pallas::Base::zero(),
            pallas::Base::zero(),
            pallas::Base::zero(),
            pallas::Base::zero(),
            voting_round_id,
        );

        let prover = MockProver::run(K, &circuit, vec![instance.to_halo2_instance()]).unwrap();

        // Should fail: derived hash ≠ witnessed vote_authority_note_old
        // (condition 2), and the circuit-derived nullifier ≠ instance
        // nullifier (condition 4, since it hashes wrong_van).
        assert!(prover.verify().is_err());
    }

    #[test]
    fn van_integrity_wrong_round_id_fails() {
        let mut rng = OsRng;

        let voting_hotkey_pk = pallas::Base::random(&mut rng);
        let total_note_value = pallas::Base::random(&mut rng);
        let voting_round_id = pallas::Base::random(&mut rng);
        let proposal_authority_old = pallas::Base::from(5u64);
        let gov_comm_rand = pallas::Base::random(&mut rng);
        let vsk_nk = pallas::Base::random(&mut rng);

        let vote_authority_note_old = van_integrity_hash(
            voting_hotkey_pk,
            total_note_value,
            voting_round_id,
            proposal_authority_old,
            gov_comm_rand,
        );

        let van_nullifier = van_nullifier_hash(vsk_nk, voting_round_id, vote_authority_note_old);
        let proposal_authority_new = proposal_authority_old - pallas::Base::one();
        let vote_authority_note_new = van_integrity_hash(
            voting_hotkey_pk,
            total_note_value,
            voting_round_id,
            proposal_authority_new,
            gov_comm_rand,
        );

        let circuit = Circuit::with_van_integrity_witnesses(
            Value::known(voting_hotkey_pk),
            Value::known(total_note_value),
            Value::known(proposal_authority_old),
            Value::known(gov_comm_rand),
            Value::known(vote_authority_note_old),
            Value::known(vsk_nk),
        );

        // Supply a DIFFERENT voting_round_id in the instance.
        let wrong_round_id = pallas::Base::random(&mut rng);
        let instance = Instance::from_parts(
            van_nullifier,
            vote_authority_note_new,
            pallas::Base::zero(),
            pallas::Base::zero(),
            pallas::Base::zero(),
            pallas::Base::zero(),
            wrong_round_id,
        );

        let prover = MockProver::run(K, &circuit, vec![instance.to_halo2_instance()]).unwrap();

        // Should fail: the voting_round_id from the instance doesn't match
        // the one hashed into the VAN (condition 2), and the nullifier's
        // inner hash uses wrong_round_id (condition 4).
        assert!(prover.verify().is_err());
    }

    /// Verifies the out-of-circuit helper produces deterministic results.
    #[test]
    fn van_integrity_hash_deterministic() {
        let mut rng = OsRng;

        let pk = pallas::Base::random(&mut rng);
        let val = pallas::Base::random(&mut rng);
        let round = pallas::Base::random(&mut rng);
        let auth = pallas::Base::random(&mut rng);
        let rand = pallas::Base::random(&mut rng);

        let h1 = van_integrity_hash(pk, val, round, auth, rand);
        let h2 = van_integrity_hash(pk, val, round, auth, rand);
        assert_eq!(h1, h2);

        // Changing any input changes the hash.
        let h3 = van_integrity_hash(pallas::Base::random(&mut rng), val, round, auth, rand);
        assert_ne!(h1, h3);
    }

    // ================================================================
    // Condition 4 (VAN Nullifier Integrity) tests
    // ================================================================

    /// Wrong VAN_NULLIFIER public input should fail condition 4.
    #[test]
    fn van_nullifier_wrong_public_input_fails() {
        let (circuit, mut instance) = make_van_integrity_test_data();

        // Corrupt the VAN nullifier public input.
        instance.van_nullifier = pallas::Base::random(&mut OsRng);

        let prover = MockProver::run(K, &circuit, vec![instance.to_halo2_instance()]).unwrap();

        // Should fail: circuit-derived nullifier ≠ corrupted instance value.
        assert!(prover.verify().is_err());
    }

    /// Using a different vsk_nk in the circuit than was used to compute
    /// the instance nullifier should fail condition 4.
    #[test]
    fn van_nullifier_wrong_vsk_nk_fails() {
        let mut rng = OsRng;

        let voting_hotkey_pk = pallas::Base::random(&mut rng);
        let total_note_value = pallas::Base::random(&mut rng);
        let voting_round_id = pallas::Base::random(&mut rng);
        let proposal_authority_old = pallas::Base::from(5u64);
        let gov_comm_rand = pallas::Base::random(&mut rng);
        let vsk_nk = pallas::Base::random(&mut rng);

        let vote_authority_note_old = van_integrity_hash(
            voting_hotkey_pk,
            total_note_value,
            voting_round_id,
            proposal_authority_old,
            gov_comm_rand,
        );

        let van_nullifier = van_nullifier_hash(vsk_nk, voting_round_id, vote_authority_note_old);
        let proposal_authority_new = proposal_authority_old - pallas::Base::one();
        let vote_authority_note_new = van_integrity_hash(
            voting_hotkey_pk,
            total_note_value,
            voting_round_id,
            proposal_authority_new,
            gov_comm_rand,
        );

        // Use a DIFFERENT vsk_nk in the circuit.
        let wrong_vsk_nk = pallas::Base::random(&mut rng);

        let circuit = Circuit::with_van_integrity_witnesses(
            Value::known(voting_hotkey_pk),
            Value::known(total_note_value),
            Value::known(proposal_authority_old),
            Value::known(gov_comm_rand),
            Value::known(vote_authority_note_old),
            Value::known(wrong_vsk_nk),
        );

        let instance = Instance::from_parts(
            van_nullifier,
            vote_authority_note_new,
            pallas::Base::zero(),
            pallas::Base::zero(),
            pallas::Base::zero(),
            pallas::Base::zero(),
            voting_round_id,
        );

        let prover = MockProver::run(K, &circuit, vec![instance.to_halo2_instance()]).unwrap();

        // Should fail: circuit computes Poseidon(wrong_vsk_nk, inner_hash)
        // which ≠ the instance van_nullifier (computed with correct vsk_nk).
        assert!(prover.verify().is_err());
    }

    /// Verifies the out-of-circuit nullifier helper produces deterministic results.
    #[test]
    fn van_nullifier_hash_deterministic() {
        let mut rng = OsRng;

        let nk = pallas::Base::random(&mut rng);
        let round = pallas::Base::random(&mut rng);
        let van = pallas::Base::random(&mut rng);

        let h1 = van_nullifier_hash(nk, round, van);
        let h2 = van_nullifier_hash(nk, round, van);
        assert_eq!(h1, h2);

        // Changing any input changes the hash.
        let h3 = van_nullifier_hash(pallas::Base::random(&mut rng), round, van);
        assert_ne!(h1, h3);
    }

    /// Verifies the domain tag is non-zero and deterministic.
    #[test]
    fn domain_van_nullifier_deterministic() {
        let d1 = domain_van_nullifier();
        let d2 = domain_van_nullifier();
        assert_eq!(d1, d2);

        // Must differ from DOMAIN_VAN (which is 0).
        assert_ne!(d1, pallas::Base::zero());
    }

    // ================================================================
    // Condition 5 (Proposal Authority Decrement) tests
    // ================================================================

    /// Proposal authority of 1 (minimum valid value) should decrement to 0.
    #[test]
    fn proposal_authority_decrement_minimum_valid() {
        let mut rng = OsRng;

        let voting_hotkey_pk = pallas::Base::random(&mut rng);
        let total_note_value = pallas::Base::random(&mut rng);
        let voting_round_id = pallas::Base::random(&mut rng);
        let proposal_authority_old = pallas::Base::one(); // minimum: decrements to 0
        let gov_comm_rand = pallas::Base::random(&mut rng);
        let vsk_nk = pallas::Base::random(&mut rng);

        let vote_authority_note_old = van_integrity_hash(
            voting_hotkey_pk,
            total_note_value,
            voting_round_id,
            proposal_authority_old,
            gov_comm_rand,
        );
        let van_nullifier = van_nullifier_hash(vsk_nk, voting_round_id, vote_authority_note_old);
        let vote_authority_note_new = van_integrity_hash(
            voting_hotkey_pk,
            total_note_value,
            voting_round_id,
            pallas::Base::zero(), // 1 - 1 = 0
            gov_comm_rand,
        );

        let circuit = Circuit::with_van_integrity_witnesses(
            Value::known(voting_hotkey_pk),
            Value::known(total_note_value),
            Value::known(proposal_authority_old),
            Value::known(gov_comm_rand),
            Value::known(vote_authority_note_old),
            Value::known(vsk_nk),
        );

        let instance = Instance::from_parts(
            van_nullifier,
            vote_authority_note_new,
            pallas::Base::zero(),
            pallas::Base::zero(),
            pallas::Base::zero(),
            pallas::Base::zero(),
            voting_round_id,
        );

        let prover = MockProver::run(K, &circuit, vec![instance.to_halo2_instance()]).unwrap();
        assert_eq!(prover.verify(), Ok(()));
    }

    /// Proposal authority of 0 should fail — voter has no remaining
    /// authority, so the range check on `diff = 0 - 1 ≈ p - 1` fails.
    #[test]
    fn proposal_authority_zero_fails() {
        let mut rng = OsRng;

        let voting_hotkey_pk = pallas::Base::random(&mut rng);
        let total_note_value = pallas::Base::random(&mut rng);
        let voting_round_id = pallas::Base::random(&mut rng);
        let proposal_authority_old = pallas::Base::zero(); // 0 — no authority left
        let gov_comm_rand = pallas::Base::random(&mut rng);
        let vsk_nk = pallas::Base::random(&mut rng);

        let vote_authority_note_old = van_integrity_hash(
            voting_hotkey_pk,
            total_note_value,
            voting_round_id,
            proposal_authority_old,
            gov_comm_rand,
        );
        let van_nullifier = van_nullifier_hash(vsk_nk, voting_round_id, vote_authority_note_old);

        // The "new" VAN doesn't matter much — the circuit should fail at
        // the range check before reaching condition 6. But we provide a
        // plausible value to isolate the failure to condition 5.
        let vote_authority_note_new = van_integrity_hash(
            voting_hotkey_pk,
            total_note_value,
            voting_round_id,
            proposal_authority_old - pallas::Base::one(), // wraps to p - 1
            gov_comm_rand,
        );

        let circuit = Circuit::with_van_integrity_witnesses(
            Value::known(voting_hotkey_pk),
            Value::known(total_note_value),
            Value::known(proposal_authority_old),
            Value::known(gov_comm_rand),
            Value::known(vote_authority_note_old),
            Value::known(vsk_nk),
        );

        let instance = Instance::from_parts(
            van_nullifier,
            vote_authority_note_new,
            pallas::Base::zero(),
            pallas::Base::zero(),
            pallas::Base::zero(),
            pallas::Base::zero(),
            voting_round_id,
        );

        let prover = MockProver::run(K, &circuit, vec![instance.to_halo2_instance()]).unwrap();

        // Should fail: diff = 0 - 1 = p - 1 ≈ 2^254, which fails the
        // 70-bit range check in condition 5.
        assert!(prover.verify().is_err());
    }

    // ================================================================
    // Condition 6 (New VAN Integrity) tests
    // ================================================================

    /// Wrong vote_authority_note_new public input should fail condition 6.
    #[test]
    fn new_van_integrity_wrong_public_input_fails() {
        let (circuit, mut instance) = make_van_integrity_test_data();

        // Corrupt the new VAN public input.
        instance.vote_authority_note_new = pallas::Base::random(&mut OsRng);

        let prover = MockProver::run(K, &circuit, vec![instance.to_halo2_instance()]).unwrap();

        // Should fail: circuit-derived new VAN ≠ corrupted instance value.
        assert!(prover.verify().is_err());
    }

    /// New VAN integrity with a large (but valid) proposal authority.
    /// Ensures the range check accepts values well within the 70-bit range.
    #[test]
    fn new_van_integrity_large_authority() {
        let mut rng = OsRng;

        let voting_hotkey_pk = pallas::Base::random(&mut rng);
        let total_note_value = pallas::Base::random(&mut rng);
        let voting_round_id = pallas::Base::random(&mut rng);
        let proposal_authority_old = pallas::Base::from(1_000_000u64);
        let gov_comm_rand = pallas::Base::random(&mut rng);
        let vsk_nk = pallas::Base::random(&mut rng);

        let vote_authority_note_old = van_integrity_hash(
            voting_hotkey_pk,
            total_note_value,
            voting_round_id,
            proposal_authority_old,
            gov_comm_rand,
        );
        let van_nullifier = van_nullifier_hash(vsk_nk, voting_round_id, vote_authority_note_old);
        let proposal_authority_new = proposal_authority_old - pallas::Base::one();
        let vote_authority_note_new = van_integrity_hash(
            voting_hotkey_pk,
            total_note_value,
            voting_round_id,
            proposal_authority_new,
            gov_comm_rand,
        );

        let circuit = Circuit::with_van_integrity_witnesses(
            Value::known(voting_hotkey_pk),
            Value::known(total_note_value),
            Value::known(proposal_authority_old),
            Value::known(gov_comm_rand),
            Value::known(vote_authority_note_old),
            Value::known(vsk_nk),
        );

        let instance = Instance::from_parts(
            van_nullifier,
            vote_authority_note_new,
            pallas::Base::zero(),
            pallas::Base::zero(),
            pallas::Base::zero(),
            pallas::Base::zero(),
            voting_round_id,
        );

        let prover = MockProver::run(K, &circuit, vec![instance.to_halo2_instance()]).unwrap();
        assert_eq!(prover.verify(), Ok(()));
    }
}
