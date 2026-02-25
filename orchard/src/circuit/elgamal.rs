//! El Gamal encryption integrity gadget for vote proof (ZKP #2).
//!
//! Proves that five ciphertext pairs (enc_share_c1_x[i], enc_share_c2_x[i]) are
//! valid El Gamal encryptions of the corresponding plaintext shares under the
//! election authority public key: C1_i = [r_i]*G, C2_i = [v_i]*G + [r_i]*ea_pk.
//!
//! Used by the vote proof circuit (Condition 11: Encryption Integrity). The
//! caller passes share cells, randomness cells, and enc_share x-coordinate cells;
//! this gadget owns all ea_pk scaffolding (witnesses ea_pk once as a
//! `NonIdentityPoint` and pins it to the instance column via `constrain_instance`)
//! and handles G via `FixedPointBaseField` (no advice-from-constant cells needed).
//!
//! Also provides out-of-circuit helpers: `spend_auth_g_affine`, `base_to_scalar`,
//! and `elgamal_encrypt` for the builder and tests.

use halo2_proofs::{
    circuit::{AssignedCell, Layouter},
    plonk::{Column, Error, Instance as InstanceColumn},
};
use pasta_curves::arithmetic::CurveAffine;
use pasta_curves::pallas;

use crate::constants::{OrchardBaseFieldBases, OrchardFixedBases};
use halo2_gadgets::ecc::{
    chip::EccChip,
    FixedPointBaseField, NonIdentityPoint, ScalarVar,
};

// ================================================================
// Instance-location descriptor
// ================================================================

/// Describes where ea_pk lives in the public-input (instance) column.
///
/// The gadget uses this to call `layouter.constrain_instance` directly
/// on the witnessed NonIdentityPoint cells, removing the need for the
/// caller to pre-allocate advice-from-instance cells and pass them down.
pub(in crate::circuit) struct EaPkInstanceLoc {
    /// The instance column that holds the public inputs.
    pub instance: Column<InstanceColumn>,
    /// Row of the ea_pk x-coordinate in the instance column.
    pub x_row: usize,
    /// Row of the ea_pk y-coordinate in the instance column.
    pub y_row: usize,
}

// ================================================================
// Out-of-circuit helpers
// ================================================================

/// Returns the SpendAuthG generator point (used as G in El Gamal).
///
/// Why SpendAuthG? El Gamal requires a prime-order generator with an unknown
/// discrete log. SpendAuthG is derived via `GroupPHash("z.cash:Orchard", "G")`
/// — a nothing-up-my-sleeve point. Using it for El Gamal (Condition 11) avoids
/// introducing a second generator point; `OrchardBaseFieldBases::SpendAuthGBase`
/// shares the same U/Z table data as the existing FullScalar SpendAuthG entry
/// (same generator and 85-window structure), though the ECC chip does register a
/// separate BaseFieldElem row-set in the proving key for the different scalar kind.
pub fn spend_auth_g_affine() -> pallas::Affine {
    use group::Curve;
    let g = crate::constants::fixed_bases::spend_auth_g::generator();
    pallas::Point::from(g).to_affine()
}

/// Converts a `pallas::Base` field element to a `pallas::Scalar`.
///
/// For small values (< 2^30) the integer representation is identical in both
/// fields. Returns `None` if the byte representation exceeds the scalar modulus.
pub fn base_to_scalar(b: pallas::Base) -> Option<pallas::Scalar> {
    use ff::PrimeField;
    pallas::Scalar::from_repr(b.to_repr()).into()
}

/// Out-of-circuit El Gamal encryption under SpendAuthG.
///
/// Computes C1 = [r]*SpendAuthG, C2 = [v]*SpendAuthG + [r]*ea_pk.
/// Returns (c1_x, c2_x). Used by the builder and tests.
pub fn elgamal_encrypt(
    share_value: pallas::Base,
    randomness: pallas::Base,
    ea_pk: pallas::Point,
) -> (pallas::Base, pallas::Base) {
    use group::Curve;

    let g = pallas::Point::from(spend_auth_g_affine());
    let r_scalar = base_to_scalar(randomness)
        .expect("randomness must be < scalar field modulus");
    let v_scalar = base_to_scalar(share_value)
        .expect("share value must be < scalar field modulus");

    let c1 = g * r_scalar;
    let c2 = g * v_scalar + ea_pk * r_scalar;

    let c1_x = *c1.to_affine().coordinates().unwrap().x();
    let c2_x = *c2.to_affine().coordinates().unwrap().x();
    (c1_x, c2_x)
}

// ================================================================
// In-circuit gadget
// ================================================================

/// Proves that for each share i, (enc_c1_x[i], enc_c2_x[i]) is a valid
/// El Gamal encryption of share_cells[i] under randomness r_cells[i] and
/// public key ea_pk: C1_i = [r_i]*G, C2_i = [v_i]*G + [r_i]*ea_pk.
///
/// G = SpendAuthG is handled via `FixedPointBaseField` (fixed-base scalar
/// multiplication using the precomputed lookup tables already loaded by the
/// circuit).
///
/// The gadget owns all ea_pk scaffolding: it witnesses the point internally and
/// calls `layouter.constrain_instance` to pin the witness to the public input
/// column described by `ea_pk_loc`. The caller need only supply the four varying
/// arrays and the ea_pk value.
pub(in crate::circuit) fn prove_elgamal_encryptions(
    ecc_chip: EccChip<OrchardFixedBases>,
    mut layouter: impl Layouter<pallas::Base>,
    namespace: &str,
    ea_pk: halo2_proofs::circuit::Value<pallas::Affine>,
    ea_pk_loc: EaPkInstanceLoc,
    share_cells: [AssignedCell<pallas::Base, pallas::Base>; 5],
    r_cells: [AssignedCell<pallas::Base, pallas::Base>; 5],
    enc_c1_cells: [AssignedCell<pallas::Base, pallas::Base>; 5],
    enc_c2_cells: [AssignedCell<pallas::Base, pallas::Base>; 5],
) -> Result<(), Error> {
    // Witness ea_pk once. NonIdentityPoint is Copy, so the value is cheaply
    // copied into each iteration's mul() call without re-witnessing.
    let ea_pk_point = NonIdentityPoint::new(
        ecc_chip.clone(),
        layouter.namespace(|| alloc::format!("{namespace} ea_pk witness")),
        ea_pk,
    )?;
    // Pin the witness directly to the public-input column.
    layouter.constrain_instance(
        ea_pk_point.inner().x().cell(),
        ea_pk_loc.instance,
        ea_pk_loc.x_row,
    )?;
    layouter.constrain_instance(
        ea_pk_point.inner().y().cell(),
        ea_pk_loc.instance,
        ea_pk_loc.y_row,
    )?;

    // SpendAuthG fixed-base descriptor — constructed once, cloned per iteration.
    // FixedPointBaseField::from_inner is cheap (wraps a chip clone + Copy enum),
    // but hoisting avoids reconstructing the descriptor 10 times across 5 shares.
    let spend_auth_g_base = FixedPointBaseField::from_inner(
        ecc_chip.clone(),
        OrchardBaseFieldBases::SpendAuthGBase,
    );

    for i in 0..5 {
        // --- C1_i = [r_i] * G ---
        //
        // G is baked into the fixed-base lookup table; no NonIdentityPoint
        // witness or constrain_equal needed for the base point.
        let c1_point = spend_auth_g_base
            .clone()
            .mul(
                layouter.namespace(|| alloc::format!("{namespace} [r_{i}] * G")),
                r_cells[i].clone(),
            )?;

        let c1_x = c1_point.extract_p().inner().clone();
        layouter.assign_region(
            || alloc::format!("{namespace} C1[{i}] x == enc_c1_x[{i}]"),
            |mut region| region.constrain_equal(c1_x.cell(), enc_c1_cells[i].cell()),
        )?;

        // --- C2_i = [v_i] * G + [r_i] * ea_pk ---

        let v_g_point = spend_auth_g_base
            .clone()
            .mul(
                layouter.namespace(|| alloc::format!("{namespace} [v_{i}] * G")),
                share_cells[i].clone(),
            )?;

        let r_i_scalar = ScalarVar::from_base(
            ecc_chip.clone(),
            layouter.namespace(|| alloc::format!("{namespace} r[{i}] to ScalarVar")),
            &r_cells[i],
        )?;
        // ea_pk_point is Copy: no new witness cells, just copies the AssignedCell
        // references for this mul.
        let (r_ea_pk_point, _) = ea_pk_point.mul(
            layouter.namespace(|| alloc::format!("{namespace} [r_{i}] * ea_pk")),
            r_i_scalar,
        )?;

        let c2_point = v_g_point.add(
            layouter.namespace(|| alloc::format!("{namespace} C2[{i}] = vG + rP")),
            &r_ea_pk_point,
        )?;

        let c2_x = c2_point.extract_p().inner().clone();
        layouter.assign_region(
            || alloc::format!("{namespace} C2[{i}] x == enc_c2_x[{i}]"),
            |mut region| region.constrain_equal(c2_x.cell(), enc_c2_cells[i].cell()),
        )?;
    }
    Ok(())
}
