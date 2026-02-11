//! C-compatible FFI functions for calling Halo2 verification from Go via CGo.
//!
//! All functions use C calling conventions and return i32 status codes:
//!   0  = success
//!   -1 = invalid input (null pointer, wrong length, etc.)
//!   -2 = verification failed (proof is invalid)
//!   -3 = internal error (deserialization, etc.)

use pasta_curves::group::ff::PrimeField;
use halo2_proofs::pasta::Fp;

use crate::toy;

/// Verify a toy circuit proof.
///
/// # Arguments
/// * `proof_ptr` - Pointer to the serialized proof bytes.
/// * `proof_len` - Length of the proof byte slice.
/// * `public_input_ptr` - Pointer to the public input (Pallas Fp, 32-byte little-endian).
/// * `public_input_len` - Length of the public input byte slice (must be 32).
///
/// # Returns
/// * `0` on successful verification.
/// * `-1` if inputs are invalid (null pointers or wrong lengths).
/// * `-2` if the proof does not verify.
/// * `-3` if there is an internal deserialization error.
///
/// # Safety
/// Caller must ensure the pointers are valid and the lengths are correct.
#[no_mangle]
pub unsafe extern "C" fn zally_verify_toy_proof(
    proof_ptr: *const u8,
    proof_len: usize,
    public_input_ptr: *const u8,
    public_input_len: usize,
) -> i32 {
    // Validate pointers and lengths.
    if proof_ptr.is_null() || public_input_ptr.is_null() {
        return -1;
    }
    if public_input_len != 32 {
        return -1;
    }
    if proof_len == 0 {
        return -1;
    }

    // Reconstruct slices from raw pointers.
    let proof = std::slice::from_raw_parts(proof_ptr, proof_len);
    let input_bytes = std::slice::from_raw_parts(public_input_ptr, public_input_len);

    // Deserialize the public input as a Pallas Fp field element (32-byte LE).
    let mut repr = [0u8; 32];
    repr.copy_from_slice(input_bytes);
    let fp_opt: Option<Fp> = Fp::from_repr(repr).into();
    let fp = match fp_opt {
        Some(f) => f,
        None => return -3,
    };

    // Run verification.
    match toy::verify_toy(proof, &fp) {
        Ok(()) => 0,
        Err(_) => -2,
    }
}
