use aether_types::{Address, H256};
use num_bigint::BigUint;
use num_traits::{One, ToPrimitive};
use serde::{Deserialize, Serialize};

/// Constant Product AMM (x * y = k)
///
/// Features:
/// - Token swaps (A <-> B)
/// - Liquidity provision (add/remove)
/// - LP tokens
/// - Fee collection (0.3%)
/// - Slippage protection
///
/// Formula:
/// - Invariant: x * y = k
/// - Swap output: dy = (dx * fee * y) / (x + dx * fee)
/// - Where fee = 0.997 (0.3% fee)
///
/// Fixed-point arithmetic:
/// - Use Q64.64 for precision
/// - All amounts in smallest unit

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiquidityPool {
    pub pool_id: H256,
    pub token_a: Address,
    pub token_b: Address,
    pub reserve_a: u128,
    pub reserve_b: u128,
    pub lp_token_supply: u128,
    pub fee_bps: u32, // Basis points (30 = 0.3%)
}

impl LiquidityPool {
    pub fn new(
        pool_id: H256,
        token_a: Address,
        token_b: Address,
        fee_bps: u32,
    ) -> Result<Self, String> {
        if fee_bps > 10000 {
            return Err("fee_bps must be <= 10000".to_string());
        }
        Ok(LiquidityPool {
            pool_id,
            token_a,
            token_b,
            reserve_a: 0,
            reserve_b: 0,
            lp_token_supply: 0,
            fee_bps,
        })
    }

    /// Add liquidity to the pool
    pub fn add_liquidity(
        &mut self,
        amount_a: u128,
        amount_b: u128,
        min_lp_tokens: u128,
    ) -> Result<u128, String> {
        if amount_a == 0 || amount_b == 0 {
            return Err("amounts must be non-zero".to_string());
        }

        let lp_tokens = if self.lp_token_supply == 0 {
            // Initial liquidity mints sqrt(amount_a * amount_b).
            let product = BigUint::from(amount_a) * BigUint::from(amount_b);
            let liquidity = integer_sqrt_biguint(&product)
                .to_u128()
                .ok_or("overflow in initial liquidity")?;

            if liquidity < 1000 {
                return Err("insufficient initial liquidity".to_string());
            }
            if liquidity < min_lp_tokens {
                return Err("insufficient LP tokens".to_string());
            }

            self.reserve_a = amount_a;
            self.reserve_b = amount_b;
            self.lp_token_supply = liquidity;

            liquidity
        } else {
            let ratio_lhs = amount_a
                .checked_mul(self.reserve_b)
                .ok_or("overflow in liquidity ratio check")?;
            let ratio_rhs = amount_b
                .checked_mul(self.reserve_a)
                .ok_or("overflow in liquidity ratio check")?;
            if ratio_lhs != ratio_rhs {
                return Err("liquidity must be added at the current pool ratio".to_string());
            }

            // Proportional liquidity — multiply before dividing for precision
            let liquidity_a = mul_div(amount_a, self.lp_token_supply, self.reserve_a)?;
            let liquidity_b = mul_div(amount_b, self.lp_token_supply, self.reserve_b)?;

            let liquidity = liquidity_a.min(liquidity_b);

            if liquidity == 0 || liquidity < min_lp_tokens {
                return Err("insufficient LP tokens".to_string());
            }

            self.reserve_a = self
                .reserve_a
                .checked_add(amount_a)
                .ok_or("reserve_a overflow")?;
            self.reserve_b = self
                .reserve_b
                .checked_add(amount_b)
                .ok_or("reserve_b overflow")?;
            self.lp_token_supply = self
                .lp_token_supply
                .checked_add(liquidity)
                .ok_or("lp_token_supply overflow")?;

            liquidity
        };

        Ok(lp_tokens)
    }

    /// Remove liquidity from the pool
    pub fn remove_liquidity(
        &mut self,
        lp_tokens: u128,
        min_amount_a: u128,
        min_amount_b: u128,
    ) -> Result<(u128, u128), String> {
        if lp_tokens == 0 {
            return Err("amount must be non-zero".to_string());
        }

        if lp_tokens > self.lp_token_supply {
            return Err("insufficient LP tokens".to_string());
        }

        let amount_a = mul_div(lp_tokens, self.reserve_a, self.lp_token_supply)
            .map_err(|e| format!("remove_liquidity amount_a: {}", e))?;
        let amount_b = mul_div(lp_tokens, self.reserve_b, self.lp_token_supply)
            .map_err(|e| format!("remove_liquidity amount_b: {}", e))?;

        if amount_a < min_amount_a || amount_b < min_amount_b {
            return Err("insufficient output amount".to_string());
        }

        self.reserve_a = self
            .reserve_a
            .checked_sub(amount_a)
            .ok_or("reserve_a underflow")?;
        self.reserve_b = self
            .reserve_b
            .checked_sub(amount_b)
            .ok_or("reserve_b underflow")?;
        self.lp_token_supply = self
            .lp_token_supply
            .checked_sub(lp_tokens)
            .ok_or("lp_token_supply underflow")?;

        Ok((amount_a, amount_b))
    }

    /// Swap token A for token B
    pub fn swap_a_to_b(&mut self, amount_in: u128, min_amount_out: u128) -> Result<u128, String> {
        if amount_in == 0 {
            return Err("amount must be non-zero".to_string());
        }

        let k_old = BigUint::from(self.reserve_a) * BigUint::from(self.reserve_b);

        let amount_out = self.get_amount_out(amount_in, self.reserve_a, self.reserve_b)?;

        if amount_out < min_amount_out {
            return Err("insufficient output amount".to_string());
        }

        self.reserve_a = self
            .reserve_a
            .checked_add(amount_in)
            .ok_or("reserve_a overflow")?;
        self.reserve_b = self
            .reserve_b
            .checked_sub(amount_out)
            .ok_or("reserve_b underflow")?;

        // Verify invariant: k must not decrease
        self.check_invariant_big(&k_old)?;

        Ok(amount_out)
    }

    /// Swap token B for token A
    pub fn swap_b_to_a(&mut self, amount_in: u128, min_amount_out: u128) -> Result<u128, String> {
        if amount_in == 0 {
            return Err("amount must be non-zero".to_string());
        }

        let k_old = BigUint::from(self.reserve_b) * BigUint::from(self.reserve_a);

        let amount_out = self.get_amount_out(amount_in, self.reserve_b, self.reserve_a)?;

        if amount_out < min_amount_out {
            return Err("insufficient output amount".to_string());
        }

        self.reserve_b = self
            .reserve_b
            .checked_add(amount_in)
            .ok_or("reserve_b overflow")?;
        self.reserve_a = self
            .reserve_a
            .checked_sub(amount_out)
            .ok_or("reserve_a underflow")?;

        // Verify invariant: k must not decrease
        self.check_invariant_big(&k_old)?;

        Ok(amount_out)
    }

    /// Calculate output amount for a swap
    /// Formula: amount_out = (amount_in * fee * reserve_out) / (reserve_in * 10000 + amount_in * fee)
    ///
    /// Uses BigUint to avoid overflow when reserves or amounts are large.
    fn get_amount_out(
        &self,
        amount_in: u128,
        reserve_in: u128,
        reserve_out: u128,
    ) -> Result<u128, String> {
        if amount_in == 0 || reserve_in == 0 || reserve_out == 0 {
            return Err("invalid reserves".to_string());
        }

        let fee_multiplier = 10000u128 - self.fee_bps as u128;
        let amount_in_with_fee = BigUint::from(amount_in) * BigUint::from(fee_multiplier);

        let numerator = &amount_in_with_fee * BigUint::from(reserve_out);
        let denominator =
            BigUint::from(reserve_in) * BigUint::from(10000u128) + &amount_in_with_fee;

        let amount_out = (&numerator / &denominator)
            .to_u128()
            .ok_or("swap output overflow")?;

        Ok(amount_out)
    }

    /// Check constant product invariant using BigUint: k_new must be >= k_old
    fn check_invariant_big(&self, k_old: &BigUint) -> Result<(), String> {
        let k_new = BigUint::from(self.reserve_a) * BigUint::from(self.reserve_b);

        if k_new == BigUint::ZERO {
            return Err("invariant violated: k = 0".to_string());
        }

        if k_new < *k_old {
            return Err("invariant violated: k decreased".to_string());
        }

        Ok(())
    }

    /// Get current price (reserve_b / reserve_a)
    pub fn get_price(&self) -> Result<u128, String> {
        if self.reserve_a == 0 {
            return Err("zero reserve".to_string());
        }

        self.reserve_b
            .checked_mul(1_000_000)
            .ok_or_else(|| "overflow in price calculation".to_string())
            .map(|n| n / self.reserve_a)
    }
}

/// Safe multiplication then division: computes a * b / c without intermediate overflow
/// when possible, falling back to an error if the product overflows u128.
fn mul_div(a: u128, b: u128, c: u128) -> Result<u128, String> {
    if c == 0 {
        return Err("division by zero in proportional calculation".to_string());
    }
    // Try direct multiplication first
    if let Some(ab) = a.checked_mul(b) {
        return Ok(ab / c);
    }
    // Overflow: use wider arithmetic via (a/c)*b + (a%c)*b/c
    let whole = (a / c)
        .checked_mul(b)
        .ok_or("overflow in proportional calculation")?;
    let remainder = (a % c)
        .checked_mul(b)
        .ok_or("overflow in proportional calculation")?
        / c;
    whole
        .checked_add(remainder)
        .ok_or_else(|| "overflow in proportional calculation".to_string())
}

fn integer_sqrt_biguint(value: &BigUint) -> BigUint {
    if value < &BigUint::from(2u8) {
        return value.clone();
    }

    let two = BigUint::from(2u8);
    let mut x = value.clone();
    let mut y = (&x + BigUint::one()) / &two;

    while y < x {
        x = y.clone();
        y = (&x + value / &x) / &two;
    }

    x
}
