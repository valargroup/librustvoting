/*
 * zally_circuits.h — C header for Zally Halo2 circuit verification FFI.
 *
 * This header declares the C-compatible functions exported by the
 * zally-circuits Rust static library (libzally_circuits.a).
 *
 * Used by Go CGo bindings in crypto/zkp/halo2/.
 */

#ifndef ZALLY_CIRCUITS_H
#define ZALLY_CIRCUITS_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/*
 * Verify a toy circuit proof (constant * a^2 * b^2 = c).
 *
 * Parameters:
 *   proof_ptr        - Pointer to serialized Halo2 proof bytes.
 *   proof_len        - Length of the proof byte array.
 *   public_input_ptr - Pointer to the public input (Pallas Fp, 32-byte LE).
 *   public_input_len - Length of the public input byte array (must be 32).
 *
 * Returns:
 *    0  on successful verification.
 *   -1  if inputs are invalid (null pointers or wrong lengths).
 *   -2  if the proof does not verify.
 *   -3  if there is an internal deserialization error.
 */
int32_t zally_verify_toy_proof(
    const uint8_t* proof_ptr,
    size_t proof_len,
    const uint8_t* public_input_ptr,
    size_t public_input_len
);

#ifdef __cplusplus
}
#endif

#endif /* ZALLY_CIRCUITS_H */
