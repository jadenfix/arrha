use std::time::{Duration, Instant};

/// Pacemaker drives round progression with exponential backoff timeouts.
///
/// When the current round's leader doesn't produce a block or enough votes
/// aren't collected, the pacemaker fires a timeout. Validators then
/// participate in a view-change protocol to elect a new leader.
pub struct Pacemaker {
    /// Base timeout for a round.
    base_timeout: Duration,
    /// Current timeout (increases on consecutive failures).
    current_timeout: Duration,
    /// Maximum timeout cap.
    max_timeout: Duration,
    /// When the current round started.
    round_start: Instant,
    /// Current round number (advances on timeout or successful commit).
    current_round: u64,
    /// Number of consecutive timeouts (reset on successful commit).
    consecutive_timeouts: u32,
}

impl Pacemaker {
    pub fn new(base_timeout: Duration) -> Self {
        Pacemaker {
            base_timeout,
            current_timeout: base_timeout,
            max_timeout: Duration::from_secs(30),
            round_start: Instant::now(),
            current_round: 0,
            consecutive_timeouts: 0,
        }
    }

    /// Check if the current round has timed out.
    pub fn is_timed_out(&self) -> bool {
        self.round_start.elapsed() >= self.current_timeout
    }

    /// Get time remaining in the current round.
    pub fn time_remaining(&self) -> Duration {
        self.current_timeout
            .checked_sub(self.round_start.elapsed())
            .unwrap_or(Duration::ZERO)
    }

    /// Advance to the next round due to a timeout.
    /// Doubles the timeout (exponential backoff) up to max.
    pub fn on_timeout(&mut self) {
        self.consecutive_timeouts = self.consecutive_timeouts.saturating_add(1);
        self.current_round = self.current_round.saturating_add(1);
        self.current_timeout = std::cmp::min(
            self.base_timeout * 2u32.pow(self.consecutive_timeouts.min(5)),
            self.max_timeout,
        );
        self.round_start = Instant::now();
    }

    /// Advance to the next round after a successful commit.
    /// Resets timeout to base.
    pub fn on_commit(&mut self) {
        self.consecutive_timeouts = 0;
        self.current_round = self.current_round.saturating_add(1);
        self.current_timeout = self.base_timeout;
        self.round_start = Instant::now();
    }

    /// Reset the round timer (e.g., when receiving a valid proposal).
    pub fn reset_timer(&mut self) {
        self.round_start = Instant::now();
    }

    pub fn current_round(&self) -> u64 {
        self.current_round
    }

    pub fn current_timeout(&self) -> Duration {
        self.current_timeout
    }

    /// Determine which validator is the leader for a given round.
    /// Simple round-robin modulo validator count.
    pub fn leader_for_round(&self, round: u64, validator_count: usize) -> usize {
        if validator_count == 0 {
            return 0;
        }
        (round as usize) % validator_count
    }

    /// Jump to at least `round`, resetting the timer.
    ///
    /// Used when a validator receives a TimeoutCertificate or a message from a
    /// future round (e.g. during sync). If `round` is already behind the current
    /// round this is a no-op.
    ///
    /// On a large jump (> 1 step) the consecutive-timeout counter is cleared so
    /// the node re-enters normal cadence rather than keeping the backed-off
    /// timeout of a node that was locally stuck.
    pub fn advance_to_round(&mut self, round: u64) {
        if round <= self.current_round {
            return;
        }
        let steps = round - self.current_round;
        self.current_round = round;
        if steps > 1 {
            // We caught up via a TC/sync — not a local stall, so reset backoff.
            self.consecutive_timeouts = 0;
            self.current_timeout = self.base_timeout;
        }
        self.round_start = Instant::now();
    }
}
