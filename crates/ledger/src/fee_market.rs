/// EIP-1559 style dynamic base fee with burn mechanism.
///
/// The base fee adjusts each block based on gas utilization:
/// - If block gas > target: base fee increases (up to 12.5% per block)
/// - If block gas < target: base fee decreases (up to 12.5% per block)
///
/// Fee distribution:
/// - Base fee is BURNED (removed from supply permanently)
/// - Priority fee (tip) goes to the block proposer
///
/// This creates deflationary pressure proportional to network usage.
///
/// Fee market state tracked per-block.
#[derive(Debug, Clone)]
pub struct FeeMarket {
    /// Current base fee per gas unit (in smallest denomination).
    pub base_fee: u128,
    /// Target gas per block (50% of max).
    pub target_gas: u64,
    /// Maximum gas per block.
    pub max_gas: u64,
    /// Minimum base fee floor.
    pub min_base_fee: u128,
    /// Total fees burned across all blocks.
    pub total_burned: u128,
    /// Total priority fees paid to proposers.
    pub total_priority_fees: u128,
    /// Total priority fees allocated to treasury.
    pub total_treasury_fees: u128,
}

/// Result of processing a block's fees.
#[derive(Debug, Clone)]
pub struct BlockFeeResult {
    /// Base fee that was burned for this block.
    pub burned: u128,
    /// Priority fees paid to the proposer (60% of tip).
    pub proposer_reward: u128,
    /// Priority fees allocated to the treasury (40% of tip).
    pub treasury_fee: u128,
    /// New base fee for the next block.
    pub next_base_fee: u128,
}

impl FeeMarket {
    pub fn new(initial_base_fee: u128, max_gas: u64, min_base_fee: u128) -> Self {
        FeeMarket {
            base_fee: initial_base_fee,
            target_gas: std::cmp::max(max_gas / 2, 1), // Prevent division by zero
            max_gas: std::cmp::max(max_gas, 2),
            min_base_fee,
            total_burned: 0,
            total_priority_fees: 0,
            total_treasury_fees: 0,
        }
    }

    /// Calculate the required minimum fee for a transaction.
    ///
    /// total_fee = base_fee * gas_limit + priority_fee
    pub fn min_fee_for_gas(&self, gas_limit: u64) -> u128 {
        self.base_fee.saturating_mul(gas_limit as u128)
    }

    /// Process a block and update the base fee.
    ///
    /// `block_gas_used`: total gas consumed by all txs in the block.
    /// `total_fees_collected`: sum of all transaction fees in the block.
    ///
    /// Returns the fee breakdown (burned vs proposer reward).
    pub fn process_block(
        &mut self,
        block_gas_used: u64,
        total_fees_collected: u128,
    ) -> BlockFeeResult {
        // Calculate burn amount (base_fee * gas_used)
        let burned = self
            .base_fee
            .saturating_mul(block_gas_used as u128)
            .min(total_fees_collected);
        // Priority fee (tip) is the remainder after the base fee burn
        let priority_fee = total_fees_collected.saturating_sub(burned);

        // Split priority fee: 60% to proposer, 40% to treasury
        let (proposer_reward, treasury_fee) =
            crate::emission::EmissionSchedule::distribute_priority_fee(priority_fee);

        self.total_burned = self.total_burned.saturating_add(burned);
        self.total_priority_fees = self.total_priority_fees.saturating_add(proposer_reward);
        self.total_treasury_fees = self.total_treasury_fees.saturating_add(treasury_fee);

        // Adjust base fee for next block (EIP-1559 formula)
        let next_base_fee = self.calculate_next_base_fee(block_gas_used);
        self.base_fee = next_base_fee;

        BlockFeeResult {
            burned,
            proposer_reward,
            treasury_fee,
            next_base_fee,
        }
    }

    /// EIP-1559 base fee adjustment algorithm.
    ///
    /// If gas_used > target: increase by up to 12.5%
    /// If gas_used < target: decrease by up to 12.5%
    /// If gas_used == target: no change
    fn calculate_next_base_fee(&self, gas_used: u64) -> u128 {
        if gas_used == self.target_gas {
            return self.base_fee;
        }

        if gas_used > self.target_gas {
            // Increase: base_fee * (1 + (gas_used - target) / target / 8)
            let gas_delta = gas_used - self.target_gas;
            let fee_delta = self.base_fee.saturating_mul(gas_delta as u128)
                / ((self.target_gas as u128).saturating_mul(8).max(1));
            // Increase by at least 1
            let fee_delta = fee_delta.max(1);
            self.base_fee.saturating_add(fee_delta)
        } else {
            // Decrease: base_fee * (1 - (target - gas_used) / target / 8)
            let gas_delta = self.target_gas - gas_used;
            let fee_delta = self.base_fee.saturating_mul(gas_delta as u128)
                / ((self.target_gas as u128).saturating_mul(8).max(1));
            let new_fee = self.base_fee.saturating_sub(fee_delta);
            new_fee.max(self.min_base_fee)
        }
    }
}
