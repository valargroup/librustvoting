//! El Gamal encryption integrity gadget for vote proof (ZKP #2).
//!
//! Proves that five ciphertext pairs (enc_share_c1_x[i], enc_share_c2_x[i]) are
//! valid El Gamal encryptions of the corresponding plaintext shares under the
//! election authority public key: C1_i = [r_i]*G, C2_i = [v_i]*G + [r_i]*ea_pk.
//!
//! Used by the vote proof circuit (Condition 11: Encryption Integrity). The
//! caller assigns ea_pk instance cells, share and randomness cells, and the
//! witnessed enc_share x-coordinates; this gadget constrains the ECC
//! computation to match those cells. G = SpendAuthG is fixed-base (no witness).
//!
//! Also provides out-of-circuit helpers: `spend_auth_g_affine`, `base_to_scalar`,
//! and `elgamal_encrypt` for the builder and tests.

use halo2_proofs::{
    circuit::{AssignedCell, Layouter},
    plonk::Error,
};
use pasta_curves::arithmetic::CurveAffine;
use pasta_curves::pallas;

use crate::constants::{OrchardBaseFieldBases, OrchardFixedBases};
use halo2_gadgets::ecc::{
    chip::EccChip,
    FixedPointBaseField, NonIdentityPoint, ScalarVar,
};

// ================================================================
// Out-of-circuit helpers
// ================================================================

/// Returns the SpendAuthG generator point (used as G in El Gamal).
///
/// Why SpendAuthG? El Gamal requires a prime-order generator with an unknown
/// discrete log. SpendAuthG is derived via `GroupPHash("z.cash:Orchard", "G")`
/// — a nothing-up-my-sleeve point. The circuit already loads SpendAuthG as a
/// fixed-base with precomputed lookup tables for spend-auth re-randomization
/// (Condition 4). Reusing it for El Gamal (Condition 11) avoids a second
/// fixed-base table, saving significant circuit size and proving time.
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
/// circuit). This eliminates the per-iteration `NonIdentityPoint::new` witness
/// and `constrain_equal` dance that the variable-base path required.
///
/// Caller must assign:
/// - `ea_pk_x_cell`, `ea_pk_y_cell`: ea_pk coordinates (advice-from-instance).
/// - `r_cells`: five advice cells holding the El Gamal randomness values.
/// - `share_cells`, `enc_c1_cells`, `enc_c2_cells`: share values and
///   ciphertext x-coordinates (already witnessed).
///
#[allow(clippy::too_many_arguments)]
pub(in crate::circuit) fn prove_elgamal_encryptions(
    ecc_chip: EccChip<OrchardFixedBases>,
    mut layouter: impl Layouter<pallas::Base>,
    namespace: &str,
    ea_pk: halo2_proofs::circuit::Value<pallas::Affine>,
    ea_pk_x_cell: AssignedCell<pallas::Base, pallas::Base>,
    ea_pk_y_cell: AssignedCell<pallas::Base, pallas::Base>,
    share_cells: [AssignedCell<pallas::Base, pallas::Base>; 5],
    r_cells: [AssignedCell<pallas::Base, pallas::Base>; 5],
    enc_c1_cells: [AssignedCell<pallas::Base, pallas::Base>; 5],
    enc_c2_cells: [AssignedCell<pallas::Base, pallas::Base>; 5],
) -> Result<(), Error> {
    for i in 0..5 {
        // --- C1_i = [r_i] * G ---
        //
        // SpendAuthG is a FixedPointBaseField base: no NonIdentityPoint witness
        // needed, the generator is baked into the lookup table.
        let c1_point = FixedPointBaseField::from_inner(
            ecc_chip.clone(),
            OrchardBaseFieldBases::SpendAuthGBase,
        )
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

        let v_g_point = FixedPointBaseField::from_inner(
            ecc_chip.clone(),
            OrchardBaseFieldBases::SpendAuthGBase,
        )
        .mul(
            layouter.namespace(|| alloc::format!("{namespace} [v_{i}] * G")),
            share_cells[i].clone(),
        )?;

        let ea_pk_point = NonIdentityPoint::new(
            ecc_chip.clone(),
            layouter.namespace(|| alloc::format!("{namespace} ea_pk for share[{i}]")),
            ea_pk,
        )?;
        layouter.assign_region(
            || alloc::format!("{namespace} constrain ea_pk[{i}] x"),
            |mut region| {
                region.constrain_equal(
                    ea_pk_point.inner().x().cell(),
                    ea_pk_x_cell.cell(),
                )
            },
        )?;
        layouter.assign_region(
            || alloc::format!("{namespace} constrain ea_pk[{i}] y"),
            |mut region| {
                region.constrain_equal(
                    ea_pk_point.inner().y().cell(),
                    ea_pk_y_cell.cell(),
                )
            },
        )?;

        let r_i_scalar = ScalarVar::from_base(
            ecc_chip.clone(),
            layouter.namespace(|| alloc::format!("{namespace} r[{i}] to ScalarVar")),
            &r_cells[i],
        )?;
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
