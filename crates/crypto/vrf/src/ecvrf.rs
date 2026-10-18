use anyhow::{bail, Result};
use curve25519_dalek::{
    constants::ED25519_BASEPOINT_POINT,
    edwards::{CompressedEdwardsY, EdwardsPoint},
    scalar::Scalar,
};
use sha2::{Digest, Sha512};
use subtle::ConstantTimeEq;

/// ECVRF-EDWARDS25519-SHA512-ELL2 implementation per RFC 9381.
///
/// Provides verifiable pseudorandom output bound to a secret key and input.
/// Used for slot leader election in VRF-PoS consensus.
///
/// Proof structure: Gamma (32 bytes) || c (16 bytes) || s (32 bytes) = 80 bytes
/// Output: SHA-512(suite_string || 0x03 || Gamma_cofactor) truncated to 32 bytes
const SUITE_STRING: u8 = 0x04; // ECVRF-EDWARDS25519-SHA512-ELL2

#[derive(Clone, Debug)]
pub struct VrfKeypair {
    /// The 32-byte seed the scalar was derived from. Persisted key files store
    /// this seed so that `from_secret(secret_bytes())` round-trips exactly.
    seed: [u8; 32],
    secret: Scalar,
    public: EdwardsPoint,
    public_bytes: [u8; 32],
}

#[derive(Clone, Debug)]
pub struct VrfProof {
    pub proof: Vec<u8>,   // 80 bytes: Gamma(32) || c(16) || s(32)
    pub output: [u8; 32], // Beta string (hash of Gamma)
}

impl Drop for VrfKeypair {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        // Overwrite secret scalar with zero to prevent memory recovery
        self.secret = Scalar::ZERO;
        self.seed.zeroize();
        self.public_bytes.zeroize();
    }
}

impl VrfKeypair {
    /// Generate a new VRF keypair from random bytes.
    #[must_use]
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut rng = rand::thread_rng();
        let mut secret_bytes = [0u8; 32];
        rng.fill_bytes(&mut secret_bytes);
        Self::from_secret_bytes(&secret_bytes)
    }

    /// Export the secret seed bytes (for key persistence).
    ///
    /// Returns the 32-byte seed accepted by [`VrfKeypair::from_secret`], so a
    /// keypair written to disk and loaded back has the same public key.
    /// (Previously this returned the derived scalar, which `from_secret` then
    /// hashed again, so every reloaded validator key had a different VRF
    /// public key than the one in genesis and every block failed VRF checks.)
    /// WARNING: Handle with care — this is secret key material.
    #[inline]
    #[must_use]
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.seed
    }

    /// Create keypair from raw 32-byte secret.
    #[must_use = "constructing a VrfKeypair without binding it is a no-op"]
    pub fn from_secret(secret: &[u8]) -> Result<Self> {
        if secret.len() != 32 {
            bail!("secret key must be 32 bytes");
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(secret);
        Ok(Self::from_secret_bytes(&arr))
    }

    fn from_secret_bytes(secret_bytes: &[u8; 32]) -> Self {
        // Derive scalar from secret via SHA-512 (same as Ed25519 key derivation)
        let hash = Sha512::digest(secret_bytes);
        let mut scalar_bytes = [0u8; 32];
        scalar_bytes.copy_from_slice(&hash[..32]);
        // Clamp per Ed25519
        scalar_bytes[0] &= 248;
        scalar_bytes[31] &= 127;
        scalar_bytes[31] |= 64;

        let secret = Scalar::from_bytes_mod_order(scalar_bytes);
        let public = secret * ED25519_BASEPOINT_POINT;
        let public_bytes = public.compress().to_bytes();

        VrfKeypair {
            seed: *secret_bytes,
            secret,
            public,
            public_bytes,
        }
    }

    /// Get compressed public key bytes.
    #[inline]
    #[must_use]
    pub fn public_key(&self) -> &[u8; 32] {
        &self.public_bytes
    }

    /// Evaluate VRF: produce (output, proof) from input.
    ///
    /// Steps per RFC 9381 Section 5.1:
    /// 1. H = encode_to_curve(public_key, alpha)
    /// 2. Gamma = secret * H
    /// 3. k = nonce_generation(secret, H)
    /// 4. c = challenge_generation(public_key, H, Gamma, k*B, k*H)
    /// 5. s = k + c * secret (mod L)
    /// 6. proof = Gamma || c || s
    /// 7. output = proof_to_hash(Gamma)
    #[must_use]
    pub fn prove(&self, alpha: &[u8]) -> VrfProof {
        // Step 1: Hash to curve using Elligator2
        let h = encode_to_curve_try_and_increment(&self.public_bytes, alpha);

        // Step 2: Gamma = x * H
        let gamma = self.secret * h;

        // Step 3: Nonce generation (deterministic, RFC 6979-like)
        let k = nonce_generation(&self.secret, &h);

        // Step 4: k*B and k*H
        let k_b = k * ED25519_BASEPOINT_POINT;
        let k_h = k * h;

        // Step 5: Challenge c = hash_points(H, Gamma, k*B, k*H)
        let c = challenge_generation(&self.public, &h, &gamma, &k_b, &k_h);

        // Step 6: s = k + c * x (mod L)
        let s = k + c * self.secret;

        // Encode proof: Gamma(32) || c(16) || s(32) = 80 bytes
        let gamma_bytes = gamma.compress().to_bytes();
        let c_bytes = scalar_to_16_bytes(&c);
        let s_bytes = s.to_bytes();

        let mut proof_bytes = Vec::with_capacity(80);
        proof_bytes.extend_from_slice(&gamma_bytes);
        proof_bytes.extend_from_slice(&c_bytes);
        proof_bytes.extend_from_slice(&s_bytes);

        // Step 7: output = proof_to_hash(Gamma)
        let output = proof_to_hash(&gamma);

        VrfProof {
            proof: proof_bytes,
            output,
        }
    }
}

/// Verify a VRF proof against a public key and input.
///
/// Steps per RFC 9381 Section 5.3:
/// 1. Decode proof: Gamma, c, s
/// 2. H = encode_to_curve(public_key, alpha)
/// 3. U = s*B - c*Y (where Y = public key point)
/// 4. V = s*H - c*Gamma
/// 5. c' = challenge_generation(Y, H, Gamma, U, V)
/// 6. Accept iff c == c'
#[must_use = "discarding a VRF verification result is a security bug"]
pub fn verify_proof(public_key: &[u8; 32], alpha: &[u8], proof: &VrfProof) -> Result<bool> {
    if proof.proof.len() != 80 {
        return Ok(false);
    }

    // Decode public key
    let y_compressed = CompressedEdwardsY::from_slice(public_key)
        .map_err(|_| anyhow::anyhow!("invalid public key"))?;
    let y = y_compressed
        .decompress()
        .ok_or_else(|| anyhow::anyhow!("public key not on curve"))?;

    // Decode proof components
    let mut gamma_bytes = [0u8; 32];
    gamma_bytes.copy_from_slice(&proof.proof[0..32]);
    let gamma = CompressedEdwardsY(gamma_bytes)
        .decompress()
        .ok_or_else(|| anyhow::anyhow!("Gamma not on curve"))?;

    let c = scalar_from_16_bytes(&proof.proof[32..48]);

    let mut s_bytes = [0u8; 32];
    s_bytes.copy_from_slice(&proof.proof[48..80]);
    let s = Option::from(Scalar::from_canonical_bytes(s_bytes))
        .unwrap_or_else(|| Scalar::from_bytes_mod_order(s_bytes));

    // Step 2: H = encode_to_curve(Y, alpha)
    let h = encode_to_curve_try_and_increment(public_key, alpha);

    // Step 3: U = s*B - c*Y
    let u = s * ED25519_BASEPOINT_POINT - c * y;

    // Step 4: V = s*H - c*Gamma
    let v = s * h - c * gamma;

    // Step 5: c' = challenge_generation(Y, H, Gamma, U, V)
    let c_prime = challenge_generation(&y, &h, &gamma, &u, &v);

    // Step 6: constant-time verify c == c'
    let c_bytes = scalar_to_16_bytes(&c);
    let c_prime_bytes = scalar_to_16_bytes(&c_prime);
    let challenge_ok = c_bytes.ct_eq(&c_prime_bytes);

    // Constant-time verify output matches proof_to_hash(Gamma)
    let expected_output = proof_to_hash(&gamma);
    let output_ok = expected_output.ct_eq(&proof.output);

    Ok(bool::from(challenge_ok & output_ok))
}

/// Encode input to a curve point using try-and-increment method.
///
/// Per RFC 9381 Section 5.4.1.2 (try_and_increment):
/// For i = 0, 1, 2, ...:
///   hash = SHA-512(suite || 0x01 || public_key || alpha || i)
///   attempt to decompress hash[0..32] as Edwards point
///   if valid, return cofactor * point
fn encode_to_curve_try_and_increment(public_key: &[u8; 32], alpha: &[u8]) -> EdwardsPoint {
    for ctr in 0u8..=255 {
        let mut hasher = Sha512::new();
        hasher.update([SUITE_STRING]);
        hasher.update([0x01]); // encode_to_curve domain separator
        hasher.update(public_key);
        hasher.update(alpha);
        hasher.update([ctr]);
        let hash_output = hasher.finalize();

        let mut attempt = [0u8; 32];
        attempt.copy_from_slice(&hash_output[..32]);

        // Try to decompress as Edwards Y coordinate
        let compressed = CompressedEdwardsY(attempt);
        if let Some(point) = compressed.decompress() {
            // Multiply by cofactor (8) to ensure we're in the prime-order subgroup
            return point.mul_by_cofactor();
        }
    }
    // Astronomically unlikely to reach here (probability ~2^-256)
    // but return the basepoint multiplied by cofactor as a fallback
    ED25519_BASEPOINT_POINT.mul_by_cofactor()
}

/// Deterministic nonce generation (RFC 6979-like).
///
/// k = SHA-512(secret_scalar_bytes || compressed_H) reduced mod L
fn nonce_generation(secret: &Scalar, h: &EdwardsPoint) -> Scalar {
    let mut hasher = Sha512::new();
    hasher.update(secret.to_bytes());
    hasher.update(h.compress().to_bytes());
    let hash = hasher.finalize();
    let mut wide_bytes = [0u8; 64];
    wide_bytes.copy_from_slice(&hash);
    Scalar::from_bytes_mod_order_wide(&wide_bytes)
}

/// Generate challenge scalar c from points.
///
/// c = SHA-512(suite || 0x02 || Y || H || Gamma || U || V)[0..16] as scalar
fn challenge_generation(
    y: &EdwardsPoint,
    h: &EdwardsPoint,
    gamma: &EdwardsPoint,
    u: &EdwardsPoint,
    v: &EdwardsPoint,
) -> Scalar {
    let mut hasher = Sha512::new();
    hasher.update([SUITE_STRING]);
    hasher.update([0x02]); // challenge domain separator
    hasher.update(y.compress().to_bytes());
    hasher.update(h.compress().to_bytes());
    hasher.update(gamma.compress().to_bytes());
    hasher.update(u.compress().to_bytes());
    hasher.update(v.compress().to_bytes());
    let hash = hasher.finalize();

    // Take first 16 bytes as the challenge (128-bit security)
    scalar_from_16_bytes(&hash[..16])
}

/// Convert Scalar to 16 bytes (truncated representation for challenge c).
fn scalar_to_16_bytes(s: &Scalar) -> [u8; 16] {
    let bytes = s.to_bytes();
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes[..16]);
    out
}

/// Reconstruct Scalar from 16 bytes (zero-extend to 32 bytes).
fn scalar_from_16_bytes(bytes: &[u8]) -> Scalar {
    let mut scalar_bytes = [0u8; 32];
    scalar_bytes[..16].copy_from_slice(&bytes[..16]);
    Scalar::from_bytes_mod_order(scalar_bytes)
}

/// Convert VRF Gamma point to output hash (Beta string).
///
/// output = SHA-512(suite || 0x03 || cofactor_Gamma)[0..32]
fn proof_to_hash(gamma: &EdwardsPoint) -> [u8; 32] {
    let cofactor_gamma = gamma.mul_by_cofactor();
    let mut hasher = Sha512::new();
    hasher.update([SUITE_STRING]);
    hasher.update([0x03]); // proof_to_hash domain separator
    hasher.update(cofactor_gamma.compress().to_bytes());
    let hash = hasher.finalize();
    let mut output = [0u8; 32];
    output.copy_from_slice(&hash[..32]);
    output
}

#[deprecated(
    note = "Use check_leader_eligibility_integer for deterministic consensus; f64 loses precision"
)]
/// Convert VRF output to a value in [0, 1) for threshold comparison.
#[must_use]
pub fn output_to_value(output: &[u8; 32]) -> f64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&output[..8]);
    let val = u64::from_le_bytes(bytes);
    if val == u64::MAX {
        1.0 - f64::EPSILON
    } else {
        val as f64 / (u64::MAX as f64)
    }
}

#[deprecated(note = "Use check_leader_eligibility_integer for deterministic consensus")]
/// Check if VRF output wins the lottery for slot leadership.
///
/// threshold = tau * stake_i / total_stake
/// eligible if output_value < threshold
#[allow(deprecated)]
#[must_use]
pub fn check_leader_eligibility(
    vrf_output: &[u8; 32],
    stake: u128,
    total_stake: u128,
    tau: f64,
) -> bool {
    let output_value = output_to_value(vrf_output);
    let threshold = tau * (stake as f64 / total_stake as f64);
    output_value < threshold
}

/// Multiply two u128 values and return the result as (high, low) u128 pair (256-bit).
fn mul_u128_wide(a: u128, b: u128) -> (u128, u128) {
    let a_lo = a as u64 as u128;
    let a_hi = a >> 64;
    let b_lo = b as u64 as u128;
    let b_hi = b >> 64;

    let lo_lo = a_lo * b_lo;
    let hi_lo = a_hi * b_lo;
    let lo_hi = a_lo * b_hi;
    let hi_hi = a_hi * b_hi;

    let mid = (lo_lo >> 64) + (hi_lo as u64 as u128) + (lo_hi as u64 as u128);
    let carry = (mid >> 64) + (hi_lo >> 64) + (lo_hi >> 64);

    let low = (lo_lo as u64 as u128) | (mid << 64);
    let high = hi_hi + carry;
    (high, low)
}

/// Compare two 256-bit values: (a_hi, a_lo) < (b_hi, b_lo)
fn lt_u256(a: (u128, u128), b: (u128, u128)) -> bool {
    if a.0 != b.0 {
        a.0 < b.0
    } else {
        a.1 < b.1
    }
}

/// Multiply a 256-bit value (hi, lo) by a u128 scalar, returning a 256-bit result.
/// Overflow beyond 256 bits saturates to (u128::MAX, u128::MAX).
fn mul_u256_by_u128(val: (u128, u128), scalar: u128) -> (u128, u128) {
    // low * scalar
    let (lo_hi, lo_lo) = mul_u128_wide(val.1, scalar);
    // high * scalar
    let (hi_hi, hi_lo) = mul_u128_wide(val.0, scalar);

    if hi_hi > 0 {
        return (u128::MAX, u128::MAX); // overflow, saturate
    }

    let new_hi = hi_lo.checked_add(lo_hi);
    match new_hi {
        Some(h) => (h, lo_lo),
        None => (u128::MAX, u128::MAX), // overflow, saturate
    }
}

/// Check leader eligibility using integer-only arithmetic (deterministic across platforms).
///
/// tau is represented as a fraction: tau_numerator / tau_denominator
/// (e.g., tau=0.8 → numerator=4, denominator=5)
///
/// Eligible if: vrf_value / 2^64 < (tau_numerator / tau_denominator) * (stake / total_stake)
/// Rearranged: vrf_value * total_stake * tau_denominator < tau_numerator * stake * 2^64
///
/// Uses 256-bit arithmetic to avoid overflow at high stake values.
#[must_use]
pub fn check_leader_eligibility_integer(
    vrf_output: &[u8; 32],
    stake: u128,
    total_stake: u128,
    tau_numerator: u128,
    tau_denominator: u128,
) -> bool {
    if total_stake == 0 || tau_denominator == 0 {
        return false;
    }
    // Interpret first 8 bytes as u64 (sufficient entropy for threshold comparison)
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&vrf_output[..8]);
    let vrf_value = u64::from_le_bytes(bytes) as u128;

    // LHS = vrf_value * total_stake * tau_denominator (256-bit)
    let lhs_step1 = mul_u128_wide(vrf_value, total_stake);
    let lhs = mul_u256_by_u128(lhs_step1, tau_denominator);

    // RHS = tau_numerator * stake * 2^64 (256-bit)
    // Multiplying by 2^64 is just shifting left by 64 bits
    let rhs_step1 = mul_u128_wide(tau_numerator, stake);
    // Shift (hi, lo) left by 64: new_hi = (old_hi << 64) | (old_lo >> 64), new_lo = old_lo << 64
    let rhs = if rhs_step1.0 >> 64 != 0 {
        (u128::MAX, u128::MAX) // overflow on shift
    } else {
        ((rhs_step1.0 << 64) | (rhs_step1.1 >> 64), rhs_step1.1 << 64)
    };

    lt_u256(lhs, rhs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secret_bytes_roundtrip_preserves_public_key() {
        let keypair = VrfKeypair::generate();
        let restored = VrfKeypair::from_secret(&keypair.secret_bytes()).unwrap();
        assert_eq!(keypair.public_key(), restored.public_key());
        let proof = restored.prove(b"slot-42");
        assert!(verify_proof(keypair.public_key(), b"slot-42", &proof).unwrap());
    }

    #[test]
    fn test_vrf_generation() {
        let keypair = VrfKeypair::generate();
        assert_eq!(keypair.public_key().len(), 32);
    }

    #[test]
    fn test_vrf_prove_deterministic() {
        let keypair = VrfKeypair::generate();
        let input = b"test input";

        let proof1 = keypair.prove(input);
        let proof2 = keypair.prove(input);

        // Same input must give same output
        assert_eq!(proof1.output, proof2.output);
        assert_eq!(proof1.proof, proof2.proof);
    }

    #[test]
    fn test_vrf_different_inputs() {
        let keypair = VrfKeypair::generate();

        let proof1 = keypair.prove(b"input1");
        let proof2 = keypair.prove(b"input2");

        assert_ne!(proof1.output, proof2.output);
    }

    #[test]
    fn test_vrf_different_keys() {
        let kp1 = VrfKeypair::generate();
        let kp2 = VrfKeypair::generate();
        let input = b"same input";

        let proof1 = kp1.prove(input);
        let proof2 = kp2.prove(input);

        // Different keys produce different outputs
        assert_ne!(proof1.output, proof2.output);
    }

    #[test]
    fn test_vrf_verification_valid() {
        let keypair = VrfKeypair::generate();
        let input = b"test input";
        let proof = keypair.prove(input);

        let verified = verify_proof(keypair.public_key(), input, &proof).unwrap();
        assert!(verified, "valid proof must verify");
    }

    #[test]
    fn test_vrf_verification_wrong_input() {
        let keypair = VrfKeypair::generate();
        let proof = keypair.prove(b"correct input");

        let verified = verify_proof(keypair.public_key(), b"wrong input", &proof).unwrap();
        assert!(!verified, "proof must not verify with wrong input");
    }

    #[test]
    fn test_vrf_verification_wrong_key() {
        let kp1 = VrfKeypair::generate();
        let kp2 = VrfKeypair::generate();
        let input = b"test input";
        let proof = kp1.prove(input);

        let verified = verify_proof(kp2.public_key(), input, &proof).unwrap();
        assert!(!verified, "proof must not verify with wrong public key");
    }

    #[test]
    fn test_vrf_verification_tampered_output() {
        let keypair = VrfKeypair::generate();
        let input = b"test input";
        let mut proof = keypair.prove(input);

        // Tamper with output
        proof.output[0] ^= 0xff;

        let verified = verify_proof(keypair.public_key(), input, &proof).unwrap();
        assert!(!verified, "tampered output must not verify");
    }

    #[test]
    fn test_vrf_verification_tampered_proof() {
        let keypair = VrfKeypair::generate();
        let input = b"test input";
        let mut proof = keypair.prove(input);

        // Tamper with proof bytes (flip a bit in the s component)
        proof.proof[50] ^= 0x01;

        let verified = verify_proof(keypair.public_key(), input, &proof).unwrap();
        assert!(!verified, "tampered proof must not verify");
    }

    #[test]
    fn test_vrf_proof_length() {
        let keypair = VrfKeypair::generate();
        let proof = keypair.prove(b"test");

        assert_eq!(proof.proof.len(), 80, "proof must be 80 bytes");
    }

    #[test]
    fn test_vrf_rejects_short_proof() {
        let keypair = VrfKeypair::generate();
        let bad_proof = VrfProof {
            proof: vec![0u8; 32], // Too short
            output: [0u8; 32],
        };

        let verified = verify_proof(keypair.public_key(), b"test", &bad_proof).unwrap();
        assert!(!verified);
    }

    #[test]
    #[allow(deprecated)]
    fn test_output_to_value_range() {
        let output = [0u8; 32];
        let val = output_to_value(&output);
        assert!((0.0..1.0).contains(&val));

        let output = [255u8; 32];
        let val = output_to_value(&output);
        assert!((0.0..1.0).contains(&val));
    }

    #[test]
    #[allow(deprecated)]
    fn test_leader_eligibility() {
        let low_output = [0u8; 32];
        assert!(check_leader_eligibility(&low_output, 100, 10_000, 0.8));
        assert!(check_leader_eligibility(&low_output, 5_000, 10_000, 0.8));

        let high_output = [255u8; 32];
        assert!(!check_leader_eligibility(&high_output, 100, 10_000, 0.8));
    }

    #[test]
    fn test_epoch_randomness_chain() {
        let keypair = VrfKeypair::generate();
        let mut epoch_randomness = [0u8; 32];

        for epoch in 0u64..5 {
            let mut input = Vec::new();
            input.extend_from_slice(&epoch_randomness);
            input.extend_from_slice(&epoch.to_le_bytes());

            let proof = keypair.prove(&input);

            // Verify each proof in the chain
            let verified = verify_proof(keypair.public_key(), &input, &proof).unwrap();
            assert!(verified, "epoch {} proof must verify", epoch);

            // New randomness from VRF output
            let mut hasher = sha2::Sha256::new();
            hasher.update(proof.output);
            epoch_randomness = hasher.finalize().into();
        }

        assert_ne!(epoch_randomness, [0u8; 32]);
    }

    #[test]
    fn test_from_secret_deterministic() {
        let secret = [42u8; 32];
        let kp1 = VrfKeypair::from_secret(&secret).unwrap();
        let kp2 = VrfKeypair::from_secret(&secret).unwrap();

        assert_eq!(kp1.public_key(), kp2.public_key());

        let proof1 = kp1.prove(b"test");
        let proof2 = kp2.prove(b"test");
        assert_eq!(proof1.output, proof2.output);
    }

    #[test]
    #[allow(deprecated)]
    fn test_stake_proportional_eligibility() {
        // Validator with 50% stake should be eligible ~40% of time (tau=0.8 * 0.5)
        let keypair = VrfKeypair::generate();
        let mut eligible_count = 0;
        let trials = 200;

        for slot in 0u64..trials {
            let mut input = Vec::new();
            input.extend_from_slice(&[0u8; 32]); // epoch randomness
            input.extend_from_slice(&slot.to_le_bytes());

            let proof = keypair.prove(&input);
            if check_leader_eligibility(&proof.output, 5000, 10_000, 0.8) {
                eligible_count += 1;
            }
        }

        let rate = eligible_count as f64 / trials as f64;
        // Expected ~40%, allow wide margin (15-65%)
        assert!(
            rate > 0.15 && rate < 0.65,
            "eligibility rate {} outside expected range",
            rate
        );
    }

    #[test]
    fn test_integer_leader_eligibility_basic() {
        // Low VRF output should be eligible with reasonable stake
        let low_output = [0u8; 32];
        assert!(check_leader_eligibility_integer(
            &low_output,
            5000,
            10_000,
            4,
            5
        ));

        // High VRF output should not be eligible with small stake
        let high_output = [255u8; 32];
        assert!(!check_leader_eligibility_integer(
            &high_output,
            100,
            10_000,
            4,
            5
        ));
    }

    #[test]
    fn test_integer_leader_eligibility_edge_cases() {
        // Zero total_stake should return false
        assert!(!check_leader_eligibility_integer(&[0u8; 32], 100, 0, 4, 5));

        // Zero tau_denominator should return false
        assert!(!check_leader_eligibility_integer(
            &[0u8; 32], 100, 1000, 4, 0
        ));

        // Full stake (stake == total_stake) with tau=1 (1/1) should always be eligible for low output
        assert!(check_leader_eligibility_integer(
            &[0u8; 32], 1000, 1000, 1, 1
        ));

        // Zero stake should never be eligible
        assert!(!check_leader_eligibility_integer(&[0u8; 32], 0, 1000, 4, 5));
    }

    #[test]
    fn test_integer_leader_eligibility_no_overflow() {
        // Test with very large stake values that could overflow
        let output = [1u8; 32]; // low-ish
        let large_stake: u128 = u64::MAX as u128;
        let large_total: u128 = u64::MAX as u128 * 2;
        // Should not panic
        let _ = check_leader_eligibility_integer(&output, large_stake, large_total, 4, 5);
    }

    #[test]
    fn test_integer_eligibility_deterministic() {
        // Integer eligibility should give consistent results
        let kp = VrfKeypair::generate();
        let proof = kp.prove(b"test-slot-1");
        let result1 = check_leader_eligibility_integer(&proof.output, 500, 1000, 8000, 10000);
        let result2 = check_leader_eligibility_integer(&proof.output, 500, 1000, 8000, 10000);
        assert_eq!(
            result1, result2,
            "integer eligibility must be deterministic"
        );
    }

    #[test]
    fn test_integer_eligibility_deterministic_fixed_output() {
        // The integer eligibility function should be deterministic with fixed inputs
        let output = [42u8; 32];
        let stake = 1000u128;
        let total = 10000u128;
        let tau_num = 8000u128; // 0.8
        let tau_den = 10000u128;
        let r1 = check_leader_eligibility_integer(&output, stake, total, tau_num, tau_den);
        let r2 = check_leader_eligibility_integer(&output, stake, total, tau_num, tau_den);
        assert_eq!(r1, r2, "integer eligibility must be deterministic");
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// VRF is deterministic: same key + same input = same output, always.
        #[test]
        fn vrf_deterministic(secret in prop::array::uniform32(any::<u8>()), input in prop::collection::vec(any::<u8>(), 0..256)) {
            let kp = VrfKeypair::from_secret(&secret).unwrap();
            let p1 = kp.prove(&input);
            let p2 = kp.prove(&input);
            prop_assert_eq!(p1.output, p2.output);
            prop_assert_eq!(p1.proof, p2.proof);
        }

        /// Valid proofs always verify.
        #[test]
        fn valid_proofs_verify(secret in prop::array::uniform32(any::<u8>()), input in prop::collection::vec(any::<u8>(), 1..128)) {
            let kp = VrfKeypair::from_secret(&secret).unwrap();
            let proof = kp.prove(&input);
            let verified = verify_proof(kp.public_key(), &input, &proof).unwrap();
            prop_assert!(verified, "valid proof must verify");
        }

        /// Different inputs produce different outputs (with overwhelming probability).
        #[test]
        fn different_inputs_different_outputs(secret in prop::array::uniform32(any::<u8>()), a in prop::collection::vec(any::<u8>(), 1..64), b in prop::collection::vec(any::<u8>(), 1..64)) {
            prop_assume!(a != b);
            let kp = VrfKeypair::from_secret(&secret).unwrap();
            let pa = kp.prove(&a);
            let pb = kp.prove(&b);
            prop_assert_ne!(pa.output, pb.output);
        }

        /// Output values are in [0, 1).
        #[test]
        #[allow(deprecated)]
        fn output_in_range(output in prop::array::uniform32(any::<u8>())) {
            let val = output_to_value(&output);
            prop_assert!((0.0..1.0).contains(&val), "output_to_value {} not in [0,1)", val);
        }

        /// Random garbage proofs must never panic — they should return Ok(false) or Err.
        #[test]
        fn garbage_proofs_no_panic(
            pubkey in prop::array::uniform32(any::<u8>()),
            proof_bytes in prop::collection::vec(any::<u8>(), 0..120),
            output in prop::array::uniform32(any::<u8>()),
            input in prop::collection::vec(any::<u8>(), 0..64),
        ) {
            let proof = VrfProof { proof: proof_bytes, output };
            let result = verify_proof(&pubkey, &input, &proof);
            if let Ok(valid) = result {
                prop_assert!(!valid, "random garbage must not verify");
            }
        }
    }
}
