use aether_types::Address;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Overflow-safe `(a * b) / c` for u128 using u256 intermediate arithmetic.
/// Returns 0 when `c == 0` (prevents division-by-zero panics in reward distribution).
///
/// `saturating_mul(b) / c` silently caps at `u128::MAX` when `a * b` exceeds 2^128,
/// producing drastically wrong results for large stakes (e.g. trillions of tokens).
/// This helper widens to 256 bits so the full product is preserved.
fn mul_div(a: u128, b: u128, c: u128) -> u128 {
    if c == 0 {
        return 0;
    }
    // Widen to (u128, u128) representing a 256-bit product via long multiplication.
    // Split each operand into two 64-bit halves to avoid overflow.
    let a_hi = a >> 64;
    let a_lo = a & 0xFFFF_FFFF_FFFF_FFFF;
    let b_hi = b >> 64;
    let b_lo = b & 0xFFFF_FFFF_FFFF_FFFF;

    let lo_lo = a_lo * b_lo;
    let hi_lo = a_hi * b_lo;
    let lo_hi = a_lo * b_hi;
    let hi_hi = a_hi * b_hi;

    // Accumulate into (high_128, low_128)
    let mid = hi_lo + (lo_lo >> 64);
    let mid = mid + lo_hi; // Note: can overflow, carry goes to hi_hi
    let carry = if mid < lo_hi { 1u128 } else { 0u128 };

    let product_lo = (mid << 64) | (lo_lo & 0xFFFF_FFFF_FFFF_FFFF);
    let product_hi = hi_hi + (mid >> 64) + carry;

    // Divide 256-bit (product_hi, product_lo) by c using long division.
    // Since c is u128 and result fits in u128 for our use cases, this is safe.
    div_256_by_128(product_hi, product_lo, c)
}

/// Divide a 256-bit number (hi, lo) by a u128 divisor, returning the u128 quotient.
/// Saturates to u128::MAX if the quotient exceeds 128 bits (shouldn't happen in
/// our reward/slash calculations since the result is a fraction of the inputs).
fn div_256_by_128(hi: u128, lo: u128, divisor: u128) -> u128 {
    if hi == 0 {
        return lo / divisor;
    }
    // If hi >= divisor, result > u128::MAX, saturate
    if hi >= divisor {
        return u128::MAX;
    }
    // Binary long division: we know hi < divisor, so quotient fits in 128 bits.
    // Process 128 bits of `lo` one bit at a time with `hi` as initial remainder.
    let mut remainder = hi;
    let mut quotient: u128 = 0;
    for i in (0..128).rev() {
        remainder = (remainder << 1) | ((lo >> i) & 1);
        if remainder >= divisor {
            remainder -= divisor;
            quotient |= 1u128 << i;
        }
    }
    quotient
}

#[derive(Debug, Error)]
pub enum StakingError {
    #[error("insufficient stake: minimum is 100 SWR (100_000_000 base units), have {got} base units (min={min})")]
    InsufficientStake { min: u128, got: u128 },
    #[error("validator already exists: {0:?}")]
    ValidatorExists(Address),
    #[error("unauthorized: caller must match validator address")]
    Unauthorized,
    #[error("validator not found: {0:?}")]
    ValidatorNotFound(Address),
    #[error("validator is not active: {0:?}")]
    ValidatorInactive(Address),
    #[error("invalid commission rate: {0} (max 10000 bps)")]
    InvalidCommission(u16),
    #[error("invalid slash rate: {0} (max 10000 bps)")]
    InvalidSlashRate(u128),
    #[error("delegation not found")]
    DelegationNotFound,
    #[error("insufficient delegation: have {have}, requested {requested}")]
    InsufficientDelegation { have: u128, requested: u128 },
    #[error("arithmetic overflow in balance calculation")]
    Overflow,
    #[error("validator is jailed until slot {until}, current slot is {current}")]
    ValidatorJailed { until: u64, current: u64 },
    #[error("validator is not jailed: {0:?}")]
    ValidatorNotJailed(Address),
    #[error("validator stake {have} below minimum {min} required to unjail")]
    UnjailInsufficientStake { have: u128, min: u128 },
}

/// Staking Program State
///
/// Manages SWR token staking for consensus security.
///
/// Features:
/// - Bond/unbond with unbonding period
/// - Delegation to validators
/// - Reward distribution
/// - Slashing for misbehavior
///
/// Economics:
/// - Min stake: 100 SWR
/// - Unbonding period: 7 days (100,800 slots)
/// - Reward rate: 5% APY
/// - Slash rate: 5% for double-sign, 0.001% per slot for downtime

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StakingState {
    /// Total staked amount
    pub total_staked: u128,

    /// All validators
    pub validators: Vec<Validator>,

    /// All delegations
    pub delegations: Vec<Delegation>,

    /// Pending unbonds
    pub unbonding: Vec<Unbonding>,

    /// Reward pool
    pub reward_pool: u128,

    /// Current epoch
    pub current_epoch: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Validator {
    pub address: Address,
    pub staked_amount: u128,
    pub delegated_amount: u128,
    pub commission_rate: u16, // Basis points (e.g., 1000 = 10%)
    pub reward_address: Address,
    pub is_active: bool,
    pub jailed_until: Option<u64>, // Slot number
    pub slash_count: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Delegation {
    pub delegator: Address,
    pub validator: Address,
    pub amount: u128,
    pub reward_debt: u128, // For reward calculation
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Unbonding {
    pub address: Address,
    pub validator: Address,
    pub amount: u128,
    pub complete_slot: u64,
}

impl StakingState {
    pub fn new() -> Self {
        StakingState {
            total_staked: 0,
            validators: Vec::new(),
            delegations: Vec::new(),
            unbonding: Vec::new(),
            reward_pool: 0,
            current_epoch: 0,
        }
    }

    /// Register a new validator.
    ///
    /// `caller` must match `address` to prevent impersonation.
    pub fn register_validator(
        &mut self,
        caller: Address,
        address: Address,
        initial_stake: u128,
        commission_rate: u16,
        reward_address: Address,
    ) -> Result<(), StakingError> {
        // Authority check: only the validator itself can register
        if caller != address {
            return Err(StakingError::Unauthorized);
        }

        const MIN_STAKE: u128 = 100_000_000; // 100 SWR with 6 decimals
        if initial_stake < MIN_STAKE {
            return Err(StakingError::InsufficientStake {
                min: MIN_STAKE,
                got: initial_stake,
            });
        }

        if self.validators.iter().any(|v| v.address == address) {
            return Err(StakingError::ValidatorExists(address));
        }

        if commission_rate > 10000 {
            return Err(StakingError::InvalidCommission(commission_rate));
        }

        let validator = Validator {
            address,
            staked_amount: initial_stake,
            delegated_amount: 0,
            commission_rate,
            reward_address,
            is_active: true,
            jailed_until: None,
            slash_count: 0,
        };

        self.validators.push(validator);
        self.total_staked = self
            .total_staked
            .checked_add(initial_stake)
            .ok_or(StakingError::Overflow)?;

        Ok(())
    }

    /// Delegate to a validator
    pub fn delegate(
        &mut self,
        caller: Address,
        delegator: Address,
        validator: Address,
        amount: u128,
    ) -> Result<(), StakingError> {
        if caller != delegator {
            return Err(StakingError::Unauthorized);
        }

        let validator_idx = self
            .validators
            .iter()
            .position(|v| v.address == validator)
            .ok_or(StakingError::ValidatorNotFound(validator))?;

        if !self.validators[validator_idx].is_active {
            return Err(StakingError::ValidatorInactive(validator));
        }

        // Create or update delegation
        if let Some(delegation) = self
            .delegations
            .iter_mut()
            .find(|d| d.delegator == delegator && d.validator == validator)
        {
            delegation.amount = delegation
                .amount
                .checked_add(amount)
                .ok_or(StakingError::Overflow)?;
        } else {
            self.delegations.push(Delegation {
                delegator,
                validator,
                amount,
                reward_debt: 0,
            });
        }

        // Update validator
        self.validators[validator_idx].delegated_amount = self.validators[validator_idx]
            .delegated_amount
            .checked_add(amount)
            .ok_or(StakingError::Overflow)?;
        self.total_staked = self
            .total_staked
            .checked_add(amount)
            .ok_or(StakingError::Overflow)?;

        Ok(())
    }

    /// Unbond stake (start unbonding period)
    pub fn unbond(
        &mut self,
        caller: Address,
        delegator: Address,
        validator: Address,
        amount: u128,
        current_slot: u64,
    ) -> Result<(), StakingError> {
        if caller != delegator {
            return Err(StakingError::Unauthorized);
        }

        let delegation = self
            .delegations
            .iter_mut()
            .find(|d| d.delegator == delegator && d.validator == validator)
            .ok_or(StakingError::DelegationNotFound)?;

        if amount > delegation.amount {
            return Err(StakingError::InsufficientDelegation {
                have: delegation.amount,
                requested: amount,
            });
        }

        // Update delegation
        delegation.amount = delegation
            .amount
            .checked_sub(amount)
            .ok_or(StakingError::Overflow)?;

        // Remove if zero
        if delegation.amount == 0 {
            self.delegations
                .retain(|d| !(d.delegator == delegator && d.validator == validator));
        }

        // Update validator
        if let Some(v) = self.validators.iter_mut().find(|v| v.address == validator) {
            v.delegated_amount = v
                .delegated_amount
                .checked_sub(amount)
                .ok_or(StakingError::Overflow)?;
        }

        // Add to unbonding queue (7 days = 100,800 slots at 500ms/slot)
        self.unbonding.push(Unbonding {
            address: delegator,
            validator,
            amount,
            complete_slot: current_slot
                .checked_add(100_800)
                .ok_or(StakingError::Overflow)?,
        });

        self.total_staked = self
            .total_staked
            .checked_sub(amount)
            .ok_or(StakingError::Overflow)?;

        Ok(())
    }

    /// Complete unbonding (transfer tokens back)
    pub fn complete_unbonding(&mut self, current_slot: u64) -> Vec<(Address, u128)> {
        let mut completed = Vec::new();

        self.unbonding.retain(|u| {
            if u.complete_slot <= current_slot {
                completed.push((u.address, u.amount));
                false
            } else {
                true
            }
        });

        completed
    }

    /// Slash a validator for misbehavior.
    ///
    /// Increments `slash_count` and jails the validator at 3 slashes with a
    /// cooldown period of 201,600 slots (~14 days). After the cooldown, the
    /// validator can call `unjail()` to reactivate if they still meet the
    /// minimum stake requirement.
    /// Unjail cooldown: 201,600 slots (~14 days at 6s/slot).
    const UNJAIL_COOLDOWN_SLOTS: u64 = 201_600;

    /// Minimum stake required to unjail (same as registration minimum: 100 SWR).
    const MIN_STAKE_TO_UNJAIL: u128 = 100_000_000;

    pub fn slash(
        &mut self,
        validator: Address,
        slash_rate: u128, // Basis points (e.g., 500 = 5%)
        current_slot: u64,
    ) -> Result<u128, StakingError> {
        if slash_rate > 10000 {
            return Err(StakingError::InvalidSlashRate(slash_rate));
        }

        let validator_idx = self
            .validators
            .iter()
            .position(|v| v.address == validator)
            .ok_or(StakingError::ValidatorNotFound(validator))?;

        // Calculate slash amount using overflow-safe 256-bit intermediate math
        let slash_amount = mul_div(
            self.validators[validator_idx].staked_amount,
            slash_rate,
            10000,
        );

        // Apply slash
        self.validators[validator_idx].staked_amount = self.validators[validator_idx]
            .staked_amount
            .saturating_sub(slash_amount);
        self.validators[validator_idx].slash_count =
            self.validators[validator_idx].slash_count.saturating_add(1);

        // Slash each delegation entry so validator aggregate, unbonding, and
        // reward distribution all operate on the same post-slash balances.
        let mut delegated_slash = 0u128;
        for delegation in self
            .delegations
            .iter_mut()
            .filter(|delegation| delegation.validator == validator)
        {
            let slash = mul_div(delegation.amount, slash_rate, 10000);
            let remaining = delegation.amount.saturating_sub(slash);
            delegated_slash =
                delegated_slash.saturating_add(delegation.amount.saturating_sub(remaining));
            delegation.amount = remaining;
        }
        self.delegations.retain(|delegation| delegation.amount > 0);
        self.validators[validator_idx].delegated_amount = self
            .delegations
            .iter()
            .filter(|delegation| delegation.validator == validator)
            .fold(0u128, |acc, delegation| {
                acc.saturating_add(delegation.amount)
            });

        // Proportionally reduce pending unbonding entries for this validator's delegators.
        // Without this, a delegator who unbonds before a slash can withdraw the full
        // pre-slash amount, effectively stealing slashed funds.
        let mut unbonding_slash = 0u128;
        for entry in self
            .unbonding
            .iter_mut()
            .filter(|u| u.validator == validator)
        {
            let slash = mul_div(entry.amount, slash_rate, 10000);
            entry.amount = entry.amount.saturating_sub(slash);
            unbonding_slash = unbonding_slash.saturating_add(slash);
        }
        self.unbonding.retain(|u| u.amount > 0);

        let total_slash = slash_amount
            .saturating_add(delegated_slash)
            .saturating_add(unbonding_slash);

        // Update total_staked to reflect slashed amounts, so reward distribution
        // and quorum calculations use the correct denominator.
        self.total_staked = self.total_staked.saturating_sub(total_slash);

        // Jail validator if slashed too many times
        if self.validators[validator_idx].slash_count >= 3 {
            self.validators[validator_idx].is_active = false;
            self.validators[validator_idx].jailed_until =
                Some(current_slot.saturating_add(Self::UNJAIL_COOLDOWN_SLOTS));
        }

        Ok(total_slash)
    }

    /// Distribute rewards proportionally to validators and their delegators.
    ///
    /// For each active validator:
    ///   1. Compute their share: epoch_rewards * (validator_stake + delegated) / total_staked
    ///   2. Validator takes commission (commission_rate bps) from that share
    ///   3. Remaining reward is distributed to delegators proportionally by delegation amount
    pub fn distribute_rewards(&mut self, epoch_rewards: u128) {
        if self.total_staked == 0 || epoch_rewards == 0 {
            return;
        }

        // Track total distributed to update total_staked after distribution
        let mut total_distributed: u128 = 0;

        // Collect validator reward info first (to avoid borrow issues)
        let validator_infos: Vec<(Address, u128, u128, u16, bool)> = self
            .validators
            .iter()
            .map(|v| {
                (
                    v.address,
                    v.staked_amount,
                    v.delegated_amount,
                    v.commission_rate,
                    v.is_active,
                )
            })
            .collect();

        for (val_addr, val_stake, delegated_amount, commission_rate, is_active) in &validator_infos
        {
            if !is_active {
                continue;
            }

            let total_stake = val_stake.saturating_add(*delegated_amount);
            if total_stake == 0 {
                continue;
            }

            let validator_reward = mul_div(epoch_rewards, total_stake, self.total_staked);
            let commission = mul_div(validator_reward, *commission_rate as u128, 10000);
            let delegator_pool = validator_reward.saturating_sub(commission);

            // Credit commission to validator
            if let Some(v) = self.validators.iter_mut().find(|v| v.address == *val_addr) {
                v.staked_amount = v.staked_amount.saturating_add(commission);
                total_distributed = total_distributed.saturating_add(commission);
            }

            // Distribute remaining rewards to delegators proportionally.
            // Track distributed amount to handle rounding remainder.
            if *delegated_amount > 0 && delegator_pool > 0 {
                let mut distributed = 0u128;
                let mut last_delegation_idx = None;
                for (idx, delegation) in self.delegations.iter_mut().enumerate() {
                    if delegation.validator == *val_addr && delegation.amount > 0 {
                        let delegator_share =
                            mul_div(delegator_pool, delegation.amount, *delegated_amount);
                        delegation.amount = delegation.amount.saturating_add(delegator_share);
                        distributed = distributed.saturating_add(delegator_share);
                        total_distributed = total_distributed.saturating_add(delegator_share);
                        last_delegation_idx = Some(idx);
                    }
                }
                // Give rounding remainder to the last delegator to prevent reward leakage
                let remainder = delegator_pool.saturating_sub(distributed);
                if remainder > 0 {
                    if let Some(idx) = last_delegation_idx {
                        self.delegations[idx].amount =
                            self.delegations[idx].amount.saturating_add(remainder);
                        total_distributed = total_distributed.saturating_add(remainder);
                    }
                }
            }
        }

        // Update total_staked to reflect distributed rewards, preventing
        // epoch-over-epoch divergence between total_staked and actual stakes.
        self.total_staked = self.total_staked.saturating_add(total_distributed);
    }

    /// Unjail a validator after the cooldown period has elapsed.
    ///
    /// Requirements:
    /// - Validator must be jailed (`is_active == false` and `jailed_until` set)
    /// - Current slot must be >= `jailed_until`
    /// - Validator must still have at least the minimum stake
    ///
    /// Resets `slash_count` to 0 and reactivates the validator.
    pub fn unjail(
        &mut self,
        caller: Address,
        validator: Address,
        current_slot: u64,
    ) -> Result<(), StakingError> {
        let v = self
            .validators
            .iter_mut()
            .find(|v| v.address == validator)
            .ok_or(StakingError::ValidatorNotFound(validator))?;

        if caller != validator {
            return Err(StakingError::Unauthorized);
        }

        let jailed_until = v
            .jailed_until
            .ok_or(StakingError::ValidatorNotJailed(validator))?;

        if v.is_active {
            return Err(StakingError::ValidatorNotJailed(validator));
        }

        if current_slot < jailed_until {
            return Err(StakingError::ValidatorJailed {
                until: jailed_until,
                current: current_slot,
            });
        }

        if v.staked_amount < Self::MIN_STAKE_TO_UNJAIL {
            return Err(StakingError::UnjailInsufficientStake {
                have: v.staked_amount,
                min: Self::MIN_STAKE_TO_UNJAIL,
            });
        }

        v.is_active = true;
        v.jailed_until = None;
        v.slash_count = 0;

        Ok(())
    }

    pub fn get_validator(&self, address: &Address) -> Option<&Validator> {
        self.validators.iter().find(|v| v.address == *address)
    }

    pub fn get_total_staked(&self) -> u128 {
        self.total_staked
    }

    pub fn active_validators(&self) -> Vec<&Validator> {
        self.validators.iter().filter(|v| v.is_active).collect()
    }
}

impl Default for StakingState {
    fn default() -> Self {
        Self::new()
    }
}
