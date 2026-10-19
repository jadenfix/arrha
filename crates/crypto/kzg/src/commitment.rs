use anyhow::{bail, Result};
use blst::{blst_fr, blst_p1, blst_p1_affine, blst_p2, blst_p2_affine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// KZG Polynomial Commitment Scheme on BLS12-381.
///
/// Provides constant-size (48-byte) commitments to polynomials and
/// constant-size opening proofs verifiable via 2 pairing checks.
///
/// Used in Aether for:
/// - AI execution trace verification (VCR)
/// - Data availability sampling
/// - Blob transactions (EIP-4844 style)

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KzgCommitment {
    pub commitment: Vec<u8>, // Compressed G1 point (48 bytes)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KzgProof {
    pub proof: Vec<u8>,      // Compressed G1 point (48 bytes)
    pub evaluation: Vec<u8>, // BLS scalar (32 bytes)
}

/// KZG trusted setup parameters (Structured Reference String).
///
/// Generated from a Powers of Tau ceremony.
/// Contains `[τ^0]_1`, `[τ^1]_1`, ..., `[τ^n]_1` in G1
/// and `[1]_2`, `[τ]_2` in G2.
pub struct TrustedSetup {
    /// G1 powers: [τ^i]_1 for i = 0..max_degree
    g1_points: Vec<blst_p1>,
    /// `[1]_2` - generator of G2
    g2_gen: blst_p2,
    /// `[τ]_2` - tau times the G2 generator
    g2_tau: blst_p2,
    /// Maximum polynomial degree
    max_degree: usize,
}

impl TrustedSetup {
    /// Generate a TESTING-ONLY trusted setup from a secret scalar.
    /// In production, use a proper Powers of Tau ceremony.
    #[must_use]
    pub fn generate_insecure(max_degree: usize, secret_tau: &[u8; 32]) -> Self {
        let tau = scalar_from_bytes(secret_tau);

        // G1 generator
        let g1_gen = g1_generator();
        // G2 generator
        let g2_gen = g2_generator();

        // Compute [τ^i]_1 for i = 0..max_degree
        let mut g1_points = Vec::with_capacity(max_degree + 1);
        let mut tau_power = scalar_one();

        for _ in 0..=max_degree {
            let point = g1_scalar_mul(&g1_gen, &tau_power);
            g1_points.push(point);
            tau_power = scalar_mul(&tau_power, &tau);
        }

        // Compute [τ]_2
        let g2_tau = g2_scalar_mul(&g2_gen, &tau);

        TrustedSetup {
            g1_points,
            g2_gen,
            g2_tau,
            max_degree,
        }
    }
}

pub struct KzgVerifier {
    setup: TrustedSetup,
}

impl KzgVerifier {
    /// Create a verifier with an insecure setup (for testing).
    #[cfg(any(test, feature = "test-utils"))]
    #[must_use]
    pub fn new_insecure_test(max_degree: usize) -> Self {
        let tau_bytes = Sha256::digest(b"aether-kzg-test-setup-DO-NOT-USE-IN-PRODUCTION");
        let mut tau = [0u8; 32];
        tau.copy_from_slice(&tau_bytes);
        KzgVerifier {
            setup: TrustedSetup::generate_insecure(max_degree, &tau),
        }
    }

    /// Create a verifier with a specific trusted setup.
    #[must_use]
    pub fn with_setup(setup: TrustedSetup) -> Self {
        KzgVerifier { setup }
    }

    /// Commit to a polynomial represented by its coefficients.
    ///
    /// Each coefficient is a 32-byte scalar (BLS12-381 field element).
    /// C = Σ_i coeff_i * [τ^i]_1
    #[must_use = "commitment result must be checked for errors"]
    pub fn commit(&self, coefficients: &[ScalarBytes]) -> Result<KzgCommitment> {
        if coefficients.is_empty() {
            bail!("empty coefficients");
        }
        if coefficients.len() > self.setup.max_degree + 1 {
            bail!(
                "degree {} exceeds maximum {}",
                coefficients.len() - 1,
                self.setup.max_degree
            );
        }

        // Multi-scalar multiplication: C = Σ coeff_i * g1_points[i]
        let commitment = multi_scalar_mul_g1(&self.setup.g1_points, coefficients);
        let compressed = compress_g1(&commitment);

        Ok(KzgCommitment {
            commitment: compressed,
        })
    }

    /// Create an opening proof that P(z) = y.
    ///
    /// Computes the quotient polynomial Q(x) = (P(x) - y) / (x - z)
    /// and returns π = [Q(τ)]_1 along with y = P(z).
    #[must_use = "proof result must be checked for errors"]
    pub fn create_proof(&self, coefficients: &[ScalarBytes], z: &ScalarBytes) -> Result<KzgProof> {
        if coefficients.is_empty() {
            bail!("empty coefficients");
        }

        // Evaluate P(z)
        let y = evaluate_polynomial(coefficients, z);

        // Compute quotient polynomial Q(x) = (P(x) - y) / (x - z)
        let quotient = compute_quotient(coefficients, z, &y);

        // Commit to quotient: π = [Q(τ)]_1
        let proof_point = multi_scalar_mul_g1(&self.setup.g1_points, &quotient);
        let compressed_proof = compress_g1(&proof_point);

        Ok(KzgProof {
            proof: compressed_proof,
            evaluation: scalar_to_bytes(&y),
        })
    }

    /// Verify that P(z) = y using the commitment C and proof π.
    ///
    /// Checks the pairing equation:
    /// `e(C - [y]_1, [1]_2) == e(π, [τ]_2 - [z]_2)`
    #[must_use = "discarding a KZG verification result is a security bug"]
    pub fn verify(
        &self,
        commitment: &KzgCommitment,
        proof: &KzgProof,
        z: &ScalarBytes,
    ) -> Result<bool> {
        if commitment.commitment.len() != 48 {
            bail!("invalid commitment length: {}", commitment.commitment.len());
        }
        if proof.proof.len() != 48 {
            bail!("invalid proof length: {}", proof.proof.len());
        }
        if proof.evaluation.len() != 32 {
            bail!("invalid evaluation length");
        }

        // Decompress points
        let c_point = decompress_g1(&commitment.commitment)?;
        let pi_point = decompress_g1(&proof.proof)?;

        // y as scalar
        let y = scalar_from_bytes(
            proof
                .evaluation
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("evaluation must be 32 bytes"))?,
        );

        // z as scalar
        let z_scalar = scalar_from_bytes(z);

        // LHS: C - [y]_1
        let g1_gen = g1_generator();
        let y_g1 = g1_scalar_mul(&g1_gen, &y);
        let lhs = g1_sub(&c_point, &y_g1);

        // RHS factor: [τ]_2 - [z]_2
        let g2_gen = self.setup.g2_gen;
        let z_g2 = g2_scalar_mul(&g2_gen, &z_scalar);
        let rhs_g2 = g2_sub(&self.setup.g2_tau, &z_g2);

        // Pairing check: e(lhs, [1]_2) == e(π, rhs_g2)
        // Equivalently: e(lhs, [1]_2) * e(-π, rhs_g2) == 1
        let valid = pairing_check(&lhs, &g2_gen, &pi_point, &rhs_g2);

        Ok(valid)
    }

    /// Batch verify multiple proofs using random linear combination.
    #[must_use = "batch verification result must be checked"]
    pub fn batch_verify(
        &self,
        commitments: &[KzgCommitment],
        proofs: &[KzgProof],
        points: &[ScalarBytes],
    ) -> Result<bool> {
        if commitments.len() != proofs.len() || proofs.len() != points.len() {
            bail!("mismatched array lengths");
        }

        // For small batches, verify individually
        for i in 0..commitments.len() {
            if !self.verify(&commitments[i], &proofs[i], &points[i])? {
                return Ok(false);
            }
        }

        Ok(true)
    }

    #[inline]
    #[must_use]
    pub fn max_degree(&self) -> usize {
        self.setup.max_degree
    }
}

/// A 32-byte scalar (BLS12-381 field element).
pub type ScalarBytes = [u8; 32];

// ============================================================
// Low-level BLS12-381 operations using the `blst` crate
// ============================================================

fn scalar_from_bytes(bytes: &[u8; 32]) -> blst_fr {
    let mut scalar = blst_fr::default();
    // SAFETY: blst_fr_from_uint64 reads exactly 4 u64s from the pointer;
    // bytes_to_u64_array returns a stack-owned [u64; 4] whose .as_ptr() is valid for the call.
    unsafe {
        blst::blst_fr_from_uint64(&mut scalar, bytes_to_u64_array(bytes).as_ptr());
    }
    scalar
}

fn scalar_to_bytes(s: &blst_fr) -> Vec<u8> {
    let mut out = [0u64; 4];
    // SAFETY: blst_uint64_from_fr writes exactly 4 u64s to the output pointer;
    // `out` is a stack-owned [u64; 4] with sufficient alignment and size.
    unsafe {
        blst::blst_uint64_from_fr(out.as_mut_ptr(), s);
    }
    let mut bytes = Vec::with_capacity(32);
    for limb in &out {
        bytes.extend_from_slice(&limb.to_le_bytes());
    }
    bytes
}

fn scalar_one() -> blst_fr {
    let mut one = [0u8; 32];
    one[0] = 1;
    scalar_from_bytes(&one)
}

fn scalar_mul(a: &blst_fr, b: &blst_fr) -> blst_fr {
    let mut result = blst_fr::default();
    // SAFETY: all three arguments are valid blst_fr values; blst_fr_mul writes to `result`.
    unsafe {
        blst::blst_fr_mul(&mut result, a, b);
    }
    result
}

fn scalar_sub(a: &blst_fr, b: &blst_fr) -> blst_fr {
    let mut result = blst_fr::default();
    // SAFETY: all three arguments are valid blst_fr values; blst_fr_sub writes to `result`.
    unsafe {
        blst::blst_fr_sub(&mut result, a, b);
    }
    result
}

fn scalar_add(a: &blst_fr, b: &blst_fr) -> blst_fr {
    let mut result = blst_fr::default();
    // SAFETY: all three arguments are valid blst_fr values; blst_fr_add writes to `result`.
    unsafe {
        blst::blst_fr_add(&mut result, a, b);
    }
    result
}

fn g1_generator() -> blst_p1 {
    // SAFETY: blst_p1_generator returns a pointer to a static constant; dereferencing
    // and copying it is always valid (the pointee is 'static and correctly aligned).
    unsafe { *blst::blst_p1_generator() }
}

fn g2_generator() -> blst_p2 {
    // SAFETY: blst_p2_generator returns a pointer to a static constant; dereferencing
    // and copying it is always valid (the pointee is 'static and correctly aligned).
    unsafe { *blst::blst_p2_generator() }
}

fn g1_scalar_mul(point: &blst_p1, scalar: &blst_fr) -> blst_p1 {
    let mut result = blst_p1::default();
    let mut scalar_bytes = [0u8; 32];
    let mut limbs = [0u64; 4];
    // SAFETY: blst_uint64_from_fr writes exactly 4 u64s to the output pointer;
    // `limbs` is a stack-owned [u64; 4] with sufficient size and alignment.
    unsafe {
        blst::blst_uint64_from_fr(limbs.as_mut_ptr(), scalar);
    }
    for (i, limb) in limbs.iter().enumerate() {
        scalar_bytes[i * 8..(i + 1) * 8].copy_from_slice(&limb.to_le_bytes());
    }
    // SAFETY: blst_p1_mult reads `point` and 256 bits (32 bytes) from `scalar_bytes`;
    // both are stack-owned with correct sizes. Writes result to `result`.
    unsafe {
        blst::blst_p1_mult(&mut result, point, scalar_bytes.as_ptr(), 256);
    }
    result
}

fn g2_scalar_mul(point: &blst_p2, scalar: &blst_fr) -> blst_p2 {
    let mut result = blst_p2::default();
    let mut scalar_bytes = [0u8; 32];
    let mut limbs = [0u64; 4];
    // SAFETY: blst_uint64_from_fr writes exactly 4 u64s; `limbs` has correct size/alignment.
    unsafe {
        blst::blst_uint64_from_fr(limbs.as_mut_ptr(), scalar);
    }
    for (i, limb) in limbs.iter().enumerate() {
        scalar_bytes[i * 8..(i + 1) * 8].copy_from_slice(&limb.to_le_bytes());
    }
    // SAFETY: blst_p2_mult reads `point` and 256 bits from `scalar_bytes`;
    // both are valid and correctly sized. Writes result to `result`.
    unsafe {
        blst::blst_p2_mult(&mut result, point, scalar_bytes.as_ptr(), 256);
    }
    result
}

fn g1_add(a: &blst_p1, b: &blst_p1) -> blst_p1 {
    let mut result = blst_p1::default();
    // SAFETY: blst_p1_to_affine converts a valid projective point to affine;
    // blst_p1_add_or_double_affine adds two valid G1 points. All arguments are
    // stack-owned with correct types.
    unsafe {
        let mut b_aff = blst_p1_affine::default();
        blst::blst_p1_to_affine(&mut b_aff, b);
        blst::blst_p1_add_or_double_affine(&mut result, a, &b_aff);
    }
    result
}

fn g1_sub(a: &blst_p1, b: &blst_p1) -> blst_p1 {
    let mut neg_b = *b;
    // SAFETY: blst_p1_cneg conditionally negates a valid G1 point in-place;
    // `neg_b` is a stack-owned copy of `b`.
    unsafe {
        blst::blst_p1_cneg(&mut neg_b, true);
    }
    g1_add(a, &neg_b)
}

fn g2_sub(a: &blst_p2, b: &blst_p2) -> blst_p2 {
    let mut neg_b = *b;
    // SAFETY: blst_p2_cneg conditionally negates a valid G2 point in-place.
    unsafe {
        blst::blst_p2_cneg(&mut neg_b, true);
    }
    let mut result = blst_p2::default();
    // SAFETY: blst_p2_to_affine and blst_p2_add_or_double_affine operate on valid
    // G2 points; all arguments are stack-owned with correct types.
    unsafe {
        let mut neg_b_aff = blst_p2_affine::default();
        blst::blst_p2_to_affine(&mut neg_b_aff, &neg_b);
        blst::blst_p2_add_or_double_affine(&mut result, a, &neg_b_aff);
    }
    result
}

fn compress_g1(point: &blst_p1) -> Vec<u8> {
    let mut compressed = [0u8; 48];
    // SAFETY: blst_p1_compress writes exactly 48 bytes to the output pointer;
    // `compressed` is a stack-owned [u8; 48] with sufficient size.
    unsafe {
        blst::blst_p1_compress(compressed.as_mut_ptr(), point);
    }
    compressed.to_vec()
}

fn decompress_g1(bytes: &[u8]) -> Result<blst_p1> {
    if bytes.len() != 48 {
        bail!("G1 point must be 48 bytes");
    }
    let mut affine = blst_p1_affine::default();
    // SAFETY: blst_p1_uncompress reads exactly 48 bytes from the pointer (length
    // validated above) and writes to `affine`. Returns an error code on invalid input.
    let err = unsafe { blst::blst_p1_uncompress(&mut affine, bytes.as_ptr()) };
    if err != blst::BLST_ERROR::BLST_SUCCESS {
        bail!("failed to decompress G1 point: {:?}", err);
    }
    let mut point = blst_p1::default();
    // SAFETY: blst_p1_from_affine converts a validated affine point (uncompress
    // succeeded above) to projective coordinates.
    unsafe {
        blst::blst_p1_from_affine(&mut point, &affine);
    }
    Ok(point)
}

/// Multi-scalar multiplication: Σ scalar_i * point_i
fn multi_scalar_mul_g1(points: &[blst_p1], scalars: &[ScalarBytes]) -> blst_p1 {
    // Zero-init is the identity (point at infinity) for blst_p1
    let mut result = blst_p1::default();

    for (i, scalar_bytes) in scalars.iter().enumerate() {
        if i >= points.len() {
            break;
        }
        let scalar = scalar_from_bytes(scalar_bytes);
        let term = g1_scalar_mul(&points[i], &scalar);
        result = g1_add(&result, &term);
    }

    result
}

/// Evaluate polynomial P(x) = Σ coefficients[i] * x^i at point z using Horner's method.
fn evaluate_polynomial(coefficients: &[ScalarBytes], z: &ScalarBytes) -> blst_fr {
    let z_scalar = scalar_from_bytes(z);
    let mut result = blst_fr::default(); // 0

    // Horner's method: start from highest degree
    for i in (0..coefficients.len()).rev() {
        result = scalar_mul(&result, &z_scalar);
        let coeff = scalar_from_bytes(&coefficients[i]);
        result = scalar_add(&result, &coeff);
    }

    result
}

/// Compute quotient polynomial Q(x) = (P(x) - y) / (x - z)
/// using synthetic division.
fn compute_quotient(
    coefficients: &[ScalarBytes],
    z: &ScalarBytes,
    y: &blst_fr,
) -> Vec<ScalarBytes> {
    let n = coefficients.len();
    if n <= 1 {
        return vec![[0u8; 32]];
    }

    let z_scalar = scalar_from_bytes(z);

    // P(x) - y: subtract y from the constant term
    let mut adjusted = Vec::with_capacity(n);
    for (i, coeff) in coefficients.iter().enumerate() {
        let c = scalar_from_bytes(coeff);
        if i == 0 {
            adjusted.push(scalar_sub(&c, y));
        } else {
            adjusted.push(c);
        }
    }

    // Synthetic division by (x - z)
    // Result has degree n-2 (one less than input)
    let mut quotient_scalars = vec![blst_fr::default(); n - 1];
    quotient_scalars[n - 2] = adjusted[n - 1]; // Leading coefficient stays

    for i in (0..n - 2).rev() {
        // q[i] = adjusted[i+1] + z * q[i+1]
        let term = scalar_mul(&z_scalar, &quotient_scalars[i + 1]);
        quotient_scalars[i] = scalar_add(&adjusted[i + 1], &term);
    }

    // Convert back to bytes
    quotient_scalars
        .iter()
        .map(|s| {
            let bytes = scalar_to_bytes(s);
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes[..32]);
            arr
        })
        .collect()
}

/// Pairing check: e(a1, a2) == e(b1, b2)
/// Implemented as: e(a1, a2) * e(-b1, b2) == 1
fn pairing_check(a1: &blst_p1, a2: &blst_p2, b1: &blst_p1, b2: &blst_p2) -> bool {
    let mut neg_b1 = *b1;
    // SAFETY: blst_p1_cneg conditionally negates a valid G1 point in-place.
    unsafe {
        blst::blst_p1_cneg(&mut neg_b1, true);
    }

    let mut a1_aff = blst_p1_affine::default();
    let mut a2_aff = blst_p2_affine::default();
    let mut neg_b1_aff = blst_p1_affine::default();
    let mut b2_aff = blst_p2_affine::default();

    // SAFETY: blst_p{1,2}_to_affine convert valid projective points to affine;
    // all arguments are stack-owned with correct types and sizes.
    unsafe {
        blst::blst_p1_to_affine(&mut a1_aff, a1);
        blst::blst_p2_to_affine(&mut a2_aff, a2);
        blst::blst_p1_to_affine(&mut neg_b1_aff, &neg_b1);
        blst::blst_p2_to_affine(&mut b2_aff, b2);
    }

    let mut pairing = blst::blst_fp12::default();
    // SAFETY: blst_miller_loop, blst_fp12_mul, blst_final_exp, and blst_fp12_is_one
    // operate on valid affine/fp12 values. All arguments are stack-owned. The pairing
    // computation is deterministic and cannot produce UB for any valid curve points.
    unsafe {
        let mut ml1 = blst::blst_fp12::default();
        blst::blst_miller_loop(&mut ml1, &a2_aff, &a1_aff);

        let mut ml2 = blst::blst_fp12::default();
        blst::blst_miller_loop(&mut ml2, &b2_aff, &neg_b1_aff);

        blst::blst_fp12_mul(&mut pairing, &ml1, &ml2);

        let mut result = blst::blst_fp12::default();
        blst::blst_final_exp(&mut result, &pairing);

        blst::blst_fp12_is_one(&result)
    }
}

fn bytes_to_u64_array(bytes: &[u8; 32]) -> [u64; 4] {
    let mut result = [0u64; 4];
    for (i, chunk) in bytes.chunks_exact(8).enumerate() {
        result[i] = u64::from_le_bytes(
            <[u8; 8]>::try_from(chunk).expect("chunks_exact(8) guarantees 8-byte slices"),
        );
    }
    result
}
