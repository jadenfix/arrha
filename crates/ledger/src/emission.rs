/// Token emission schedule for SWR staking rewards.
///
/// The emission rate decreases over time:
/// - Year 1: 5% annual inflation
/// - Year 2: 4%
/// - Year 3: 3%
/// - Year 4: 2%
/// - Year 5+: 1% (terminal rate)
///
/// Rewards are distributed per-epoch to validators proportional to their stake.
///
/// Fee distribution (from EIP-1559 fee market):
/// - Base fee × gas_used: BURNED (removed from supply)
/// - 60% of priority fee (tip): to block proposer
/// - 40% of priority fee (tip): to treasury
///
/// Default slots per year (500ms slots × 2 per second × 86400 seconds/day × 365 days).
/// Prefer using EmissionSchedule fields which are derived from ChainConfig.
pub const DEFAULT_SLOTS_PER_YEAR: u64 = 63_072_000;

/// Default slots per epoch (6 hours = 43,200 slots at 500ms).
pub const DEFAULT_SLOTS_PER_EPOCH: u64 = 43_200;

/// Default epochs per year.
pub const DEFAULT_EPOCHS_PER_YEAR: u64 = DEFAULT_SLOTS_PER_YEAR / DEFAULT_SLOTS_PER_EPOCH; // ~1,460

/// Emission schedule: calculates per-epoch rewards.
#[derive(Debug, Clone)]
pub struct EmissionSchedule {
    /// Initial total supply of SWR.
    pub initial_supply: u128,
    /// Genesis slot (when the chain started).
    pub genesis_slot: u64,
    /// Slots per year (derived from chain config).
    pub slots_per_year: u64,
    /// Slots per epoch (derived from chain config).
    pub slots_per_epoch: u64,
    /// Epochs per year (derived from chain config).
    pub epochs_per_year: u64,
}

impl EmissionSchedule {
    /// Create from chain config parameters.
    pub fn new(initial_supply: u128, slot_ms: u64, epoch_slots: u64) -> Self {
        let slot_ms = slot_ms.max(1); // prevent division by zero
        let epoch_slots = epoch_slots.max(1); // prevent division by zero
        let slots_per_year = (365 * 24 * 3600 * 1000) / slot_ms;
        let epochs_per_year = (slots_per_year / epoch_slots).max(1); // prevent division by zero
        EmissionSchedule {
            initial_supply,
            genesis_slot: 0,
            slots_per_year,
            slots_per_epoch: epoch_slots,
            epochs_per_year,
        }
    }

    /// Create with default timing (500ms slots, 43200 slot epochs).
    pub fn with_defaults(initial_supply: u128) -> Self {
        Self::new(initial_supply, 500, 43_200)
    }

    /// Get the annual inflation rate (basis points) for a given year.
    ///
    /// Year 0 (first year): 500 bps (5%)
    /// Year 1: 400 bps (4%)
    /// Year 2: 300 bps (3%)
    /// Year 3: 200 bps (2%)
    /// Year 4+: 100 bps (1%) — terminal rate
    pub fn annual_rate_bps(&self, year: u64) -> u64 {
        match year {
            0 => 500,
            1 => 400,
            2 => 300,
            3 => 200,
            _ => 100, // Terminal rate: 1%
        }
    }

    /// Which year does this slot fall in?
    pub fn year_for_slot(&self, slot: u64) -> u64 {
        slot.saturating_sub(self.genesis_slot) / self.slots_per_year
    }

    /// Calculate total emission for one epoch.
    ///
    /// epoch_emission = (annual_rate / 10000) * total_supply / epochs_per_year
    pub fn epoch_emission(&self, current_slot: u64, total_supply: u128) -> u128 {
        let year = self.year_for_slot(current_slot);
        let rate_bps = self.annual_rate_bps(year) as u128;

        // annual_emission = total_supply * rate_bps / 10_000
        // epoch_emission = annual_emission / epochs_per_year
        let denominator = 10_000u128
            .saturating_mul(self.epochs_per_year as u128)
            .max(1);
        total_supply.saturating_mul(rate_bps) / denominator
    }

    /// Fee distribution: split priority fees between proposer and treasury.
    pub fn distribute_priority_fee(priority_fee: u128) -> (u128, u128) {
        let proposer_share = priority_fee.saturating_mul(60) / 100; // 60% to proposer
        let treasury_share = priority_fee.saturating_sub(proposer_share); // 40% to treasury
        (proposer_share, treasury_share)
    }
}
