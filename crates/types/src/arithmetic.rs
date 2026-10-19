//! Integer arithmetic shared by consensus and token accounting.

/// Compute `floor(a * b / divisor)` with the complete 256-bit product.
/// Returns zero for a zero divisor and saturates only if the quotient exceeds
/// `u128::MAX`. Reward callers use a stake denominator at least as large as b.
pub fn saturating_mul_div(a: u128, b: u128, divisor: u128) -> u128 {
    if divisor == 0 {
        return 0;
    }
    const MASK: u128 = u64::MAX as u128;
    let low = (a & MASK) * (b & MASK);
    let cross_a = (a >> 64) * (b & MASK);
    let cross_b = (a & MASK) * (b >> 64);
    // Add only 64-bit limbs here: summing the full cross-products can overflow.
    let middle = (low >> 64) + (cross_a & MASK) + (cross_b & MASK);
    let hi = (a >> 64) * (b >> 64) + (cross_a >> 64) + (cross_b >> 64) + (middle >> 64);
    let lo = (middle << 64) | (low & MASK);
    if hi == 0 {
        return lo / divisor;
    }
    if hi >= divisor {
        return u128::MAX;
    }
    let mut remainder = hi;
    let mut quotient = 0;
    for bit in (0..128).rev() {
        // Preserve the 129th bit of the shifted remainder before truncation.
        let carry = remainder >> 127 != 0;
        remainder = (remainder << 1) | ((lo >> bit) & 1);
        if carry || remainder >= divisor {
            remainder = remainder.wrapping_sub(divisor);
            quotient |= 1u128 << bit;
        }
    }
    quotient
}
