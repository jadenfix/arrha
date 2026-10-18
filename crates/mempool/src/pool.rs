use aether_metrics::MEMPOOL_METRICS;
use aether_types::{Address, FeeParams, Transaction, H256};
use anyhow::Result;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::time::Instant;

const MAX_MEMPOOL_SIZE: usize = 50_000;
const MIN_FEE: u128 = 1000;
const MAX_TXS_PER_SENDER_PER_SECOND: u32 = 100;
const RATE_LIMIT_WINDOW_SECS: u64 = 1;
/// Maximum queued (future-nonce) transactions per sender.
const MAX_QUEUED_PER_SENDER: usize = 64;
/// Maximum nonce gap from the expected nonce.
const MAX_NONCE_GAP: u64 = 256;
/// Txs waiting longer than this many slots with sufficient fee must be included.
const FORCED_INCLUSION_SLOTS: u64 = 10;
/// Maximum age (in slots) before a transaction is evicted from the mempool.
/// At ~2s slots this is ~1 hour.
const MAX_TX_AGE_SLOTS: u64 = 1800;

#[derive(Clone)]
struct PrioritizedTx {
    tx: Transaction,
    fee_rate: u128,
    timestamp: u64,
    /// Slot when the tx entered the mempool (for forced inclusion tracking).
    submitted_slot: u64,
}

impl PartialEq for PrioritizedTx {
    fn eq(&self, other: &Self) -> bool {
        self.tx.hash() == other.tx.hash()
    }
}

impl Eq for PrioritizedTx {}

impl PartialOrd for PrioritizedTx {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PrioritizedTx {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.fee_rate.cmp(&other.fee_rate) {
            Ordering::Equal => other.timestamp.cmp(&self.timestamp),
            other => other,
        }
    }
}

/// Rate limit tracker per sender.
struct RateLimitEntry {
    window_start: Instant,
    count: u32,
}

pub struct Mempool {
    /// Priority queue for pending (ready-to-execute) transactions.
    pending: BinaryHeap<PrioritizedTx>,
    /// Quick lookup by hash.
    by_hash: HashMap<H256, Transaction>,
    /// Txs grouped by sender.
    by_sender: HashMap<Address, HashSet<H256>>,
    /// Next expected nonce per sender (from chain state).
    next_nonce: HashMap<Address, u64>,
    /// Queued transactions: future nonces waiting for gaps to fill.
    /// sender → nonce → (Transaction, submitted_slot)
    queued: HashMap<Address, BTreeMap<u64, (Transaction, u64)>>,
    /// Per-sender rate limiting.
    rate_limits: HashMap<Address, RateLimitEntry>,
    /// Monotonic counter for FIFO tiebreaking.
    current_time: u64,
    /// Current slot number (updated externally for forced inclusion tracking).
    current_slot: u64,
    /// Fee parameters for validating transaction fees.
    fee_params: FeeParams,
    /// Expected chain ID for replay protection (0 = no validation).
    expected_chain_id: u64,
}

impl Mempool {
    pub fn new(fee_params: FeeParams, expected_chain_id: u64) -> Self {
        Mempool {
            pending: BinaryHeap::new(),
            by_hash: HashMap::new(),
            by_sender: HashMap::new(),
            next_nonce: HashMap::new(),
            queued: HashMap::new(),
            rate_limits: HashMap::new(),
            current_time: 0,
            current_slot: 0,
            fee_params,
            expected_chain_id,
        }
    }

    /// Create with devnet fee defaults (convenience for tests).
    pub fn with_defaults() -> Self {
        let config = aether_types::ChainConfig::devnet();
        Self::new(config.fees, config.chain.chain_id_numeric)
    }

    /// Update the current slot (for forced inclusion age tracking).
    /// Also expires transactions older than `MAX_TX_AGE_SLOTS`.
    pub fn set_current_slot(&mut self, slot: u64) {
        self.current_slot = slot;
        self.expire_old_transactions();
    }

    /// Remove transactions that have been in the mempool longer than `MAX_TX_AGE_SLOTS`.
    /// This prevents indefinite accumulation from senders whose nonces never advance.
    fn expire_old_transactions(&mut self) {
        let mut expired_hashes = Vec::new();

        // Expire from pending heap
        for ptx in self.pending.iter() {
            let age = self.current_slot.saturating_sub(ptx.submitted_slot);
            if age > MAX_TX_AGE_SLOTS {
                expired_hashes.push(ptx.tx.hash());
            }
        }

        // Expire from queued (future-nonce) transactions
        let senders: Vec<Address> = self.queued.keys().cloned().collect();
        for sender in senders {
            if let Some(nonces) = self.queued.get(&sender) {
                let stale: Vec<u64> = nonces
                    .iter()
                    .filter(|(_, (_, slot))| {
                        self.current_slot.saturating_sub(*slot) > MAX_TX_AGE_SLOTS
                    })
                    .map(|(nonce, _)| *nonce)
                    .collect();
                for nonce in &stale {
                    if let Some(nonces_mut) = self.queued.get_mut(&sender) {
                        if let Some((tx, _)) = nonces_mut.remove(nonce) {
                            expired_hashes.push(tx.hash());
                        }
                    }
                }
            }
            if self.queued.get(&sender).is_none_or(|q| q.is_empty()) {
                self.queued.remove(&sender);
            }
        }

        if !expired_hashes.is_empty() {
            let count = expired_hashes.len() as u64;
            // Remove from by_hash and by_sender, rebuild pending heap
            self.remove_transactions(&expired_hashes);
            MEMPOOL_METRICS.evictions_total.inc_by(count);
        }
    }

    /// Set the expected next nonce for a sender (from chain state).
    pub fn set_sender_nonce(&mut self, sender: Address, nonce: u64) {
        self.next_nonce.insert(sender, nonce);
    }

    /// Advance the expected nonce for a sender to at least `min_nonce`.
    /// Unlike `set_sender_nonce`, this never moves the nonce backward — useful
    /// when processing a block whose transactions may arrive out of order.
    pub fn advance_sender_nonce(&mut self, sender: Address, min_nonce: u64) {
        let current = self.next_nonce.get(&sender).copied().unwrap_or(0);
        if min_nonce > current {
            self.next_nonce.insert(sender, min_nonce);
        }
    }

    /// Add a transaction to the mempool with nonce ordering and rate limiting.
    pub fn add_transaction(&mut self, tx: Transaction) -> Result<()> {
        let _span = tracing::debug_span!(
            "mempool_add_tx",
            fee = tx.fee,
            nonce = tx.nonce,
            pool_size = self.by_hash.len(),
        )
        .entered();
        // Reject cross-chain transactions (replay protection)
        if self.expected_chain_id != 0 && tx.chain_id != self.expected_chain_id {
            MEMPOOL_METRICS.rejected_total.inc();
            anyhow::bail!(
                "chain_id mismatch: tx has {}, expected {}",
                tx.chain_id,
                self.expected_chain_id
            );
        }

        tx.verify_signature().map_err(|e| {
            MEMPOOL_METRICS.rejected_total.inc();
            anyhow::anyhow!("invalid signature: {}", e)
        })?;

        tx.calculate_fee(&self.fee_params).map_err(|e| {
            MEMPOOL_METRICS.rejected_total.inc();
            anyhow::anyhow!("invalid fee: {}", e)
        })?;

        if tx.fee < MIN_FEE {
            MEMPOOL_METRICS.rejected_total.inc();
            anyhow::bail!("fee below minimum");
        }

        // Rate limiting
        if let Err(e) = self.check_rate_limit(&tx.sender) {
            MEMPOOL_METRICS.rate_limited_total.inc();
            MEMPOOL_METRICS.rejected_total.inc();
            return Err(e);
        }

        let tx_hash = tx.hash();

        // Exact duplicate check
        if self.by_hash.contains_key(&tx_hash) {
            MEMPOOL_METRICS.rejected_total.inc();
            anyhow::bail!("duplicate transaction");
        }

        // Replace-by-fee: if the same sender already has a tx with the same nonce,
        // allow replacement only if the new fee is >10% higher.
        if let Some(existing_hashes) = self.by_sender.get(&tx.sender) {
            let same_nonce_hash = existing_hashes
                .iter()
                .find(|h| self.by_hash.get(h).is_some_and(|t| t.nonce == tx.nonce))
                .copied();
            if let Some(old_hash) = same_nonce_hash {
                let old_fee = self.by_hash[&old_hash].fee;
                let min_replacement_fee = old_fee.saturating_add(old_fee / 10);
                if tx.fee <= min_replacement_fee {
                    MEMPOOL_METRICS.rejected_total.inc();
                    anyhow::bail!(
                        "fee {} not high enough to replace (need >10% above {})",
                        tx.fee,
                        old_fee
                    );
                }
                MEMPOOL_METRICS.rbf_replacements_total.inc();
                let old_nonce = self.by_hash[&old_hash].nonce;
                // Remove the old transaction being replaced
                self.by_hash.remove(&old_hash);
                if let Some(sender_txs) = self.by_sender.get_mut(&tx.sender) {
                    sender_txs.remove(&old_hash);
                }
                // If the replaced tx was already pending (nonce < next_nonce),
                // roll back next_nonce so the replacement can enter pending.
                let expected = self.next_nonce.get(&tx.sender).copied().unwrap_or(0);
                if old_nonce < expected {
                    self.next_nonce.insert(tx.sender, old_nonce);
                }
                // Stale heap entry is harmless — skipped in get_transactions() via by_hash check
            }
        }

        // Capacity check
        if self.by_hash.len() >= MAX_MEMPOOL_SIZE {
            self.evict_lowest_fee();
        }

        // Nonce-based routing
        let expected_nonce = self.next_nonce.get(&tx.sender).copied().unwrap_or(0);

        if tx.nonce < expected_nonce {
            MEMPOOL_METRICS.rejected_total.inc();
            anyhow::bail!(
                "nonce too low: tx nonce {} < expected {}",
                tx.nonce,
                expected_nonce
            );
        }

        // Track in by_hash and by_sender
        self.by_hash.insert(tx_hash, tx.clone());
        self.by_sender.entry(tx.sender).or_default().insert(tx_hash);

        if tx.nonce == expected_nonce {
            // Ready to execute — add to pending
            self.add_to_pending(tx);
            // Promote any queued txs that are now sequential
            self.promote_queued(tx_hash);
        } else {
            // Future nonce — enforce per-sender limits to prevent DoS
            let nonce_gap = tx.nonce.saturating_sub(expected_nonce);
            if nonce_gap > MAX_NONCE_GAP {
                self.by_hash.remove(&tx_hash);
                if let Some(sender_txs) = self.by_sender.get_mut(&tx.sender) {
                    sender_txs.remove(&tx_hash);
                    if sender_txs.is_empty() {
                        self.by_sender.remove(&tx.sender);
                    }
                }
                MEMPOOL_METRICS.rejected_total.inc();
                anyhow::bail!(
                    "nonce gap too large: tx nonce {} is {} ahead of expected {}",
                    tx.nonce,
                    nonce_gap,
                    expected_nonce
                );
            }

            let sender_queued = self.queued.entry(tx.sender).or_default();
            if sender_queued.len() >= MAX_QUEUED_PER_SENDER {
                self.by_hash.remove(&tx_hash);
                if let Some(sender_txs) = self.by_sender.get_mut(&tx.sender) {
                    sender_txs.remove(&tx_hash);
                    if sender_txs.is_empty() {
                        self.by_sender.remove(&tx.sender);
                    }
                }
                MEMPOOL_METRICS.rejected_total.inc();
                anyhow::bail!(
                    "too many queued transactions for sender (max {})",
                    MAX_QUEUED_PER_SENDER
                );
            }
            sender_queued.insert(tx.nonce, (tx, self.current_slot));
        }

        MEMPOOL_METRICS.admitted_total.inc();
        self.update_gauges();
        Ok(())
    }

    /// Promote queued transactions that are now sequential after a nonce advancement.
    fn promote_queued(&mut self, _trigger_hash: H256) {
        // Collect senders that might have promotable txs
        let senders: Vec<Address> = self.queued.keys().cloned().collect();

        for sender in senders {
            loop {
                let expected = self.next_nonce.get(&sender).copied().unwrap_or(0);
                let should_promote = self
                    .queued
                    .get(&sender)
                    .and_then(|q| q.get(&expected))
                    .is_some();

                if !should_promote {
                    break;
                }

                let (tx, _submitted_slot) = match self
                    .queued
                    .get_mut(&sender)
                    .and_then(|q| q.remove(&expected))
                {
                    Some(tx) => tx,
                    None => break,
                };

                self.add_to_pending(tx);

                // Clean up empty queued maps
                if self.queued.get(&sender).is_none_or(|q| q.is_empty()) {
                    self.queued.remove(&sender);
                }
            }
        }
    }

    fn add_to_pending(&mut self, tx: Transaction) {
        let tx_size = bincode::serialize(&tx)
            .map(|b| b.len() as u128)
            .unwrap_or(1); // Fallback to 1 to avoid divide-by-zero
        let fee_rate = tx.fee.checked_div(tx_size).unwrap_or(tx.fee);

        // Advance expected nonce
        let sender = tx.sender;
        let next = tx.nonce.saturating_add(1);
        let current_expected = self.next_nonce.get(&sender).copied().unwrap_or(0);
        if next > current_expected {
            self.next_nonce.insert(sender, next);
        }

        self.pending.push(PrioritizedTx {
            tx,
            fee_rate,
            timestamp: self.current_time,
            submitted_slot: self.current_slot,
        });
        self.current_time = self.current_time.saturating_add(1);
    }

    /// Handle a chain reorg: re-add reverted txs, remove invalid ones.
    pub fn reorg(&mut self, reverted_txs: Vec<Transaction>, new_tip_nonces: HashMap<Address, u64>) {
        MEMPOOL_METRICS.reorgs_total.inc();
        // Reset nonces to the new chain tip
        for (sender, nonce) in &new_tip_nonces {
            self.next_nonce.insert(*sender, *nonce);
        }

        // Remove any pending txs with nonces below the new chain tip
        // (these were already executed in the surviving chain)
        let mut stale_hashes = Vec::new();
        for (hash, tx) in &self.by_hash {
            let tip_nonce = new_tip_nonces.get(&tx.sender).copied().unwrap_or(0);
            if tx.nonce < tip_nonce {
                stale_hashes.push(*hash);
            }
        }
        self.remove_transactions(&stale_hashes);

        // Clear rate limits during reorg (reverted txs are legitimate)
        self.rate_limits.clear();

        // Re-add reverted transactions (they're no longer in a block)
        for tx in reverted_txs {
            if let Err(e) = self.add_transaction(tx) {
                // Log at module level since tracing may not be available
                tracing::warn!(err = %e, "failed to re-add reverted tx during reorg");
            }
        }
    }

    fn check_rate_limit(&mut self, sender: &Address) -> Result<()> {
        let now = Instant::now();

        // Prune stale rate limit entries (older than 60s) to bound memory
        if self.rate_limits.len() > 10_000 {
            self.rate_limits
                .retain(|_, e| now.duration_since(e.window_start).as_secs() < 60);
        }

        let entry = self.rate_limits.entry(*sender).or_insert(RateLimitEntry {
            window_start: now,
            count: 0,
        });

        if now.duration_since(entry.window_start).as_secs() >= RATE_LIMIT_WINDOW_SECS {
            entry.window_start = now;
            entry.count = 1;
            Ok(())
        } else {
            entry.count += 1;
            if entry.count > MAX_TXS_PER_SENDER_PER_SECOND {
                anyhow::bail!("rate limited: too many transactions from sender");
            }
            Ok(())
        }
    }

    pub fn get_transactions(&mut self, max_count: usize, max_gas: u64) -> Vec<Transaction> {
        let mut selected = Vec::new();
        let mut total_gas = 0u64;
        let mut temp_heap = BinaryHeap::new();

        while let Some(ptx) = self.pending.pop() {
            if selected.len() >= max_count || total_gas >= max_gas {
                temp_heap.push(ptx);
                break;
            }

            if total_gas.saturating_add(ptx.tx.gas_limit) <= max_gas {
                let tx_hash = ptx.tx.hash();
                if self.by_hash.contains_key(&tx_hash) {
                    selected.push(ptx.tx.clone());
                    total_gas = total_gas.saturating_add(ptx.tx.gas_limit);
                }
            } else {
                temp_heap.push(ptx);
            }
        }

        while let Some(ptx) = temp_heap.pop() {
            self.pending.push(ptx);
        }

        selected
    }

    /// Highest-priority ready transactions WITHOUT removing them from the
    /// pool. Block producers use this: a transaction must stay available
    /// until its block is final, because the block may lose to a fork.
    pub fn peek_transactions(&self, max_count: usize) -> Vec<Transaction> {
        let mut ready: Vec<&PrioritizedTx> = self
            .pending
            .iter()
            .filter(|p| self.by_hash.contains_key(&p.tx.hash()))
            .collect();
        ready.sort_by(|a, b| b.cmp(a));
        let mut seen = HashSet::new();
        ready
            .into_iter()
            .filter(|p| seen.insert(p.tx.hash()))
            .take(max_count)
            .map(|p| p.tx.clone())
            .collect()
    }

    /// Return transactions that MUST be included (anti-censorship).
    /// A tx must be included if it has waited > FORCED_INCLUSION_SLOTS
    /// and pays >= 2x the base_fee (clearly willing to pay market rate).
    pub fn must_include_transactions(&self, current_slot: u64, base_fee: u128) -> Vec<Transaction> {
        let min_fee = base_fee.saturating_mul(2);
        let mut forced = Vec::new();

        // Iterate pending heap without consuming (peek at all items)
        for ptx in self.pending.iter() {
            let age = current_slot.saturating_sub(ptx.submitted_slot);
            if age >= FORCED_INCLUSION_SLOTS && ptx.tx.fee >= min_fee {
                forced.push(ptx.tx.clone());
            }
        }

        forced
    }

    pub fn remove_transactions(&mut self, tx_hashes: &[H256]) {
        let mut removed = 0u64;
        for hash in tx_hashes {
            if let Some(tx) = self.by_hash.remove(hash) {
                if let Some(sender_txs) = self.by_sender.get_mut(&tx.sender) {
                    sender_txs.remove(hash);
                }
                removed += 1;
            }
        }
        self.rebuild_heap();
        MEMPOOL_METRICS.removed_total.inc_by(removed);
        self.update_gauges();
    }

    fn rebuild_heap(&mut self) {
        let mut new_heap = BinaryHeap::new();
        while let Some(ptx) = self.pending.pop() {
            if self.by_hash.contains_key(&ptx.tx.hash()) {
                new_heap.push(ptx);
            }
        }
        self.pending = new_heap;
    }

    fn evict_lowest_fee(&mut self) {
        MEMPOOL_METRICS.evictions_total.inc();
        // Prefer evicting queued (future-nonce) txs over ready-to-execute pending txs.
        let mut worst_queued: Option<(Address, u64, u128)> = None;
        for (sender, nonces) in &self.queued {
            for (nonce, (tx, _slot)) in nonces {
                let dominated = worst_queued.as_ref().is_none_or(|(_, _, f)| tx.fee < *f);
                if dominated {
                    worst_queued = Some((*sender, *nonce, tx.fee));
                }
            }
        }

        if let Some((sender, nonce, _)) = worst_queued {
            if let Some(nonces) = self.queued.get_mut(&sender) {
                if let Some((tx, _slot)) = nonces.remove(&nonce) {
                    let tx_hash = tx.hash();
                    self.by_hash.remove(&tx_hash);
                    if let Some(sender_txs) = self.by_sender.get_mut(&sender) {
                        sender_txs.remove(&tx_hash);
                    }
                }
                if nonces.is_empty() {
                    self.queued.remove(&sender);
                }
            }
            return;
        }

        // Fall back to evicting lowest-fee pending tx
        let mut txs: Vec<_> = std::mem::take(&mut self.pending).into_vec();
        txs.sort_by(|a, b| b.cmp(a));
        if let Some(lowest) = txs.pop() {
            let tx_hash = lowest.tx.hash();
            self.by_hash.remove(&tx_hash);
            if let Some(sender_txs) = self.by_sender.get_mut(&lowest.tx.sender) {
                sender_txs.remove(&tx_hash);
            }
        }
        self.pending = txs.into();
    }

    pub fn len(&self) -> usize {
        self.by_hash.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    /// Update Prometheus gauge metrics to reflect current pool state.
    fn update_gauges(&self) {
        MEMPOOL_METRICS.pool_size.set(self.by_hash.len() as i64);
        MEMPOOL_METRICS.pending_size.set(self.pending.len() as i64);
        MEMPOOL_METRICS.queued_size.set(self.queued_len() as i64);
    }

    /// Number of queued (future nonce) transactions.
    pub fn queued_len(&self) -> usize {
        self.queued.values().map(|q| q.len()).sum()
    }
}

impl Default for Mempool {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_crypto_primitives::Keypair;
    use aether_types::{PublicKey, Signature};

    fn create_test_tx_with_keypair(kp: &Keypair, nonce: u64, fee: u128) -> Transaction {
        let sender_pubkey = PublicKey::from_bytes(kp.public_key().to_vec());
        let sender = sender_pubkey.to_address();
        let mut tx = Transaction {
            nonce,
            chain_id: 900, // devnet chain_id_numeric
            sender,
            sender_pubkey,
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21000,
            fee,
            signature: Signature::from_bytes(vec![]),
        };

        let hash = tx.hash();
        let signature = kp.sign(hash.as_bytes());
        tx.signature = Signature::from_bytes(signature);
        tx
    }

    fn create_test_tx(nonce: u64, fee: u128) -> Transaction {
        let kp = Keypair::generate();
        create_test_tx_with_keypair(&kp, nonce, fee)
    }

    #[test]
    fn peek_transactions_does_not_remove() {
        let mut pool = Mempool::with_defaults();
        let tx = create_test_tx(0, 5_000_000);
        pool.add_transaction(tx).unwrap();
        assert_eq!(pool.peek_transactions(10).len(), 1);
        assert_eq!(pool.peek_transactions(10).len(), 1);
        assert_eq!(pool.get_transactions(10, u64::MAX).len(), 1);
    }

    #[test]
    fn test_add_transaction() {
        let mut mempool = Mempool::with_defaults();
        let tx = create_test_tx(0, 60_000);
        mempool.add_transaction(tx).unwrap();
        assert_eq!(mempool.len(), 1);
    }

    #[test]
    fn test_priority_ordering() {
        let mut mempool = Mempool::with_defaults();

        // All nonce 0 from different senders — all go to pending
        let tx1 = create_test_tx(0, 110_000);
        let tx2 = create_test_tx(0, 160_000);
        let tx3 = create_test_tx(0, 130_000);

        mempool.add_transaction(tx1).unwrap();
        mempool.add_transaction(tx2).unwrap();
        mempool.add_transaction(tx3).unwrap();

        let txs = mempool.get_transactions(10, 1_000_000);
        assert_eq!(txs[0].fee, 160_000);
        assert_eq!(txs[1].fee, 130_000);
        assert_eq!(txs[2].fee, 110_000);
    }

    #[test]
    fn test_gas_limit() {
        let mut mempool = Mempool::with_defaults();
        let tx1 = create_test_tx(0, 90_000);
        let tx2 = create_test_tx(1, 120_000);

        mempool.add_transaction(tx1).unwrap();
        mempool.add_transaction(tx2).unwrap();

        let txs = mempool.get_transactions(10, 25000);
        assert_eq!(txs.len(), 1);
    }

    #[test]
    fn test_remove_transactions() {
        let mut mempool = Mempool::with_defaults();
        let tx1 = create_test_tx(0, 90_000);
        let tx2 = create_test_tx(1, 120_000);

        mempool.add_transaction(tx1.clone()).unwrap();
        mempool.add_transaction(tx2).unwrap();

        mempool.remove_transactions(&[tx1.hash()]);
        assert_eq!(mempool.len(), 1);
    }

    #[test]
    fn test_nonce_ordering_sequential() {
        let kp = Keypair::generate();
        let mut mempool = Mempool::with_defaults();

        // Nonces 0, 1, 2 in order — all go to pending
        let tx0 = create_test_tx_with_keypair(&kp, 0, 60_000);
        let tx1 = create_test_tx_with_keypair(&kp, 1, 60_000);
        let tx2 = create_test_tx_with_keypair(&kp, 2, 60_000);

        mempool.add_transaction(tx0).unwrap();
        mempool.add_transaction(tx1).unwrap();
        mempool.add_transaction(tx2).unwrap();

        assert_eq!(mempool.len(), 3);
        assert_eq!(mempool.queued_len(), 0);
    }

    #[test]
    fn test_nonce_gap_queues_future() {
        let kp = Keypair::generate();
        let mut mempool = Mempool::with_defaults();

        // Submit nonce 0, then skip to nonce 5
        let tx0 = create_test_tx_with_keypair(&kp, 0, 60_000);
        let tx5 = create_test_tx_with_keypair(&kp, 5, 60_000);

        mempool.add_transaction(tx0).unwrap();
        mempool.add_transaction(tx5).unwrap();

        assert_eq!(mempool.len(), 2); // Both tracked
        assert_eq!(mempool.queued_len(), 1); // nonce 5 is queued
    }

    #[test]
    fn test_nonce_too_low_rejected() {
        let kp = Keypair::generate();
        let mut mempool = Mempool::with_defaults();

        let sender_pubkey = PublicKey::from_bytes(kp.public_key().to_vec());
        let sender = sender_pubkey.to_address();
        mempool.set_sender_nonce(sender, 5);

        // Nonce 3 is below expected 5 — rejected
        let tx = create_test_tx_with_keypair(&kp, 3, 60_000);
        let result = mempool.add_transaction(tx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nonce too low"));
    }

    #[test]
    fn test_queued_promotion() {
        let kp = Keypair::generate();
        let mut mempool = Mempool::with_defaults();

        // Submit nonces out of order: 0, 3, 1, 2
        let tx0 = create_test_tx_with_keypair(&kp, 0, 60_000);
        let tx3 = create_test_tx_with_keypair(&kp, 3, 60_000);
        let tx1 = create_test_tx_with_keypair(&kp, 1, 60_000);
        let tx2 = create_test_tx_with_keypair(&kp, 2, 60_000);

        mempool.add_transaction(tx0).unwrap();
        assert_eq!(mempool.queued_len(), 0);

        mempool.add_transaction(tx3).unwrap();
        assert_eq!(mempool.queued_len(), 1); // nonce 3 queued

        mempool.add_transaction(tx1).unwrap();
        assert_eq!(mempool.queued_len(), 1); // nonce 3 still queued

        mempool.add_transaction(tx2).unwrap();
        // nonce 2 fills the gap → nonce 3 should be promoted
        assert_eq!(mempool.queued_len(), 0, "all txs should be promoted");
        assert_eq!(mempool.len(), 4);
    }

    #[test]
    fn test_reorg_readds_reverted_txs() {
        let kp = Keypair::generate();
        let mut mempool = Mempool::with_defaults();

        let tx0 = create_test_tx_with_keypair(&kp, 0, 60_000);
        let tx1 = create_test_tx_with_keypair(&kp, 1, 60_000);

        let sender = tx0.sender;

        // Verify the txs are valid before reorg
        assert!(tx0.verify_signature().is_ok(), "tx0 sig should be valid");
        assert!(tx1.verify_signature().is_ok(), "tx1 sig should be valid");

        // Reorg: block reverted, txs come back
        let mut new_nonces = HashMap::new();
        new_nonces.insert(sender, 0);

        // Test add_transaction directly first
        let mut mempool2 = Mempool::with_defaults();
        let r = mempool2.add_transaction(tx0.clone());
        assert!(r.is_ok(), "tx0 add failed: {:?}", r.err());
        let r = mempool2.add_transaction(tx1.clone());
        assert!(r.is_ok(), "tx1 add failed: {:?}", r.err());
        assert_eq!(mempool2.len(), 2);

        // Now test via reorg
        mempool.reorg(vec![tx0, tx1], new_nonces);
        assert_eq!(mempool.len(), 2, "reverted txs should be back in pool");
    }

    #[test]
    fn test_rate_limiting() {
        let kp = Keypair::generate();
        let mut mempool = Mempool::with_defaults();

        // Submit 100 txs — should succeed
        for i in 0..100u64 {
            let tx = create_test_tx_with_keypair(&kp, i, 60_000);
            mempool.add_transaction(tx).unwrap();
        }

        // 101st should be rate limited
        let tx = create_test_tx_with_keypair(&kp, 100, 60_000);
        let result = mempool.add_transaction(tx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rate limited"));
    }

    #[test]
    fn test_chain_id_mismatch_rejected() {
        let mut mempool = Mempool::with_defaults();
        let kp = Keypair::generate();
        let sender_pubkey = PublicKey::from_bytes(kp.public_key().to_vec());
        let sender = sender_pubkey.to_address();

        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1, // wrong chain_id (devnet expects 900)
            sender,
            sender_pubkey,
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21000,
            fee: 60_000,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(kp.sign(hash.as_bytes()));

        let result = mempool.add_transaction(tx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("chain_id mismatch"),
            "wrong chain_id should be rejected"
        );
    }

    #[test]
    fn test_eviction_cleans_queued_map() {
        let mut mempool = Mempool::with_defaults();
        let kp = Keypair::generate();

        // Add nonce 0 (pending) and nonce 5 (queued)
        let tx0 = create_test_tx_with_keypair(&kp, 0, 60_000);
        let tx5 = create_test_tx_with_keypair(&kp, 5, 60_000);
        mempool.add_transaction(tx0).unwrap();
        mempool.add_transaction(tx5).unwrap();
        assert_eq!(mempool.queued_len(), 1);

        // Evict should remove the queued tx first (lower priority than pending)
        mempool.evict_lowest_fee();
        assert_eq!(mempool.queued_len(), 0, "eviction should clean queued map");
        assert_eq!(mempool.len(), 1, "only pending tx should remain");
    }

    #[test]
    fn test_eviction_prefers_lowest_fee_queued() {
        let mut mempool = Mempool::with_defaults();
        let kp1 = Keypair::generate();
        let kp2 = Keypair::generate();

        // kp1: nonce 0 pending, nonce 5 queued with high fee
        mempool
            .add_transaction(create_test_tx_with_keypair(&kp1, 0, 60_000))
            .unwrap();
        mempool
            .add_transaction(create_test_tx_with_keypair(&kp1, 5, 200_000))
            .unwrap();

        // kp2: nonce 0 pending, nonce 3 queued with low fee
        mempool
            .add_transaction(create_test_tx_with_keypair(&kp2, 0, 60_000))
            .unwrap();
        mempool
            .add_transaction(create_test_tx_with_keypair(&kp2, 3, 60_000))
            .unwrap();

        assert_eq!(mempool.queued_len(), 2);
        assert_eq!(mempool.len(), 4);

        mempool.evict_lowest_fee();

        // Should evict kp2's queued tx (lower fee: 60k < 200k)
        assert_eq!(mempool.queued_len(), 1);
        assert_eq!(mempool.len(), 3);
    }

    #[test]
    fn test_advance_sender_nonce_only_moves_forward() {
        let kp = Keypair::generate();
        let sender_pubkey = PublicKey::from_bytes(kp.public_key().to_vec());
        let sender = sender_pubkey.to_address();
        let mut mempool = Mempool::with_defaults();

        // Advance from 0 → 5
        mempool.advance_sender_nonce(sender, 5);
        let tx = create_test_tx_with_keypair(&kp, 3, 60_000);
        let result = mempool.add_transaction(tx);
        assert!(result.is_err(), "nonce 3 < 5 should be rejected");
        assert!(result.unwrap_err().to_string().contains("nonce too low"));

        // Trying to move backward (to 2) should be a no-op
        mempool.advance_sender_nonce(sender, 2);
        let tx = create_test_tx_with_keypair(&kp, 4, 60_000);
        let result = mempool.add_transaction(tx);
        assert!(
            result.is_err(),
            "nonce 4 < 5 should still be rejected after backward advance"
        );
    }

    #[test]
    fn test_advance_nonce_rejects_replayed_block_txs() {
        let kp = Keypair::generate();
        let mut mempool = Mempool::with_defaults();

        // Simulate: block contained tx with nonce 0 from this sender
        let sender_pubkey = PublicKey::from_bytes(kp.public_key().to_vec());
        let sender = sender_pubkey.to_address();
        mempool.advance_sender_nonce(sender, 1); // nonce 0 consumed

        // Attempt to add nonce 0 (replay) — must fail
        let tx = create_test_tx_with_keypair(&kp, 0, 60_000);
        let result = mempool.add_transaction(tx);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nonce too low"));

        // Nonce 1 should succeed
        let tx = create_test_tx_with_keypair(&kp, 1, 60_000);
        assert!(mempool.add_transaction(tx).is_ok());
    }

    #[test]
    fn test_per_sender_queued_limit() {
        let mut mempool = Mempool::with_defaults();
        let kp = Keypair::generate();

        mempool
            .add_transaction(create_test_tx_with_keypair(&kp, 0, 60_000))
            .unwrap();

        for i in 0..MAX_QUEUED_PER_SENDER {
            let nonce = (i as u64) + 2;
            mempool
                .add_transaction(create_test_tx_with_keypair(&kp, nonce, 60_000))
                .unwrap();
        }
        assert_eq!(mempool.queued_len(), MAX_QUEUED_PER_SENDER);

        let overflow_nonce = (MAX_QUEUED_PER_SENDER as u64) + 2;
        let result =
            mempool.add_transaction(create_test_tx_with_keypair(&kp, overflow_nonce, 60_000));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too many queued"));
    }

    #[test]
    fn test_nonce_gap_too_large_rejected() {
        let mut mempool = Mempool::with_defaults();
        let kp = Keypair::generate();

        mempool
            .add_transaction(create_test_tx_with_keypair(&kp, 0, 60_000))
            .unwrap();

        let far_nonce = MAX_NONCE_GAP + 2;
        let result = mempool.add_transaction(create_test_tx_with_keypair(&kp, far_nonce, 60_000));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("nonce gap too large"));
    }

    #[test]
    fn test_ttl_expires_pending_transactions() {
        let mut mempool = Mempool::with_defaults();
        mempool.set_current_slot(100);

        let tx1 = create_test_tx(0, 60_000);
        mempool.add_transaction(tx1).unwrap();
        assert_eq!(mempool.len(), 1);

        // Advance slot past TTL
        mempool.set_current_slot(100 + MAX_TX_AGE_SLOTS + 1);
        assert_eq!(mempool.len(), 0, "expired pending tx should be evicted");
    }

    #[test]
    fn test_ttl_expires_queued_transactions() {
        let mut mempool = Mempool::with_defaults();
        let kp = Keypair::generate();
        mempool.set_current_slot(50);

        // Add nonce 0 (pending) and nonce 2 (queued, gap at nonce 1)
        mempool
            .add_transaction(create_test_tx_with_keypair(&kp, 0, 60_000))
            .unwrap();
        mempool
            .add_transaction(create_test_tx_with_keypair(&kp, 2, 60_000))
            .unwrap();
        assert_eq!(mempool.queued_len(), 1);
        assert_eq!(mempool.len(), 2);

        // Advance slot past TTL — both should expire
        mempool.set_current_slot(50 + MAX_TX_AGE_SLOTS + 1);
        assert_eq!(mempool.len(), 0, "all expired txs should be evicted");
        assert_eq!(
            mempool.queued_len(),
            0,
            "queued expired txs should be evicted"
        );
    }

    #[test]
    fn test_ttl_keeps_fresh_transactions() {
        let mut mempool = Mempool::with_defaults();
        mempool.set_current_slot(100);

        let tx1 = create_test_tx(0, 60_000);
        mempool.add_transaction(tx1).unwrap();

        // Advance within TTL
        mempool.set_current_slot(100 + MAX_TX_AGE_SLOTS - 1);
        assert_eq!(mempool.len(), 1, "fresh tx should not be evicted");
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use aether_crypto_primitives::Keypair;
    use aether_types::{PublicKey, Signature};
    use proptest::prelude::*;

    fn make_signed_tx(kp: &Keypair, nonce: u64, fee: u128, chain_id: u64) -> Transaction {
        let sender_pubkey = PublicKey::from_bytes(kp.public_key().to_vec());
        let sender = sender_pubkey.to_address();
        let mut tx = Transaction {
            nonce,
            chain_id,
            sender,
            sender_pubkey,
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21000,
            fee,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(kp.sign(hash.as_bytes()));
        tx
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(50))]

        /// Pool size never exceeds the number of successfully added transactions.
        #[test]
        fn pool_size_bounded_by_adds(
            fees in proptest::collection::vec(50_000u128..500_000, 1..20),
        ) {
            let kp = Keypair::generate();
            let mut mempool = Mempool::with_defaults();
            let mut added = 0usize;

            for (i, fee) in fees.iter().enumerate() {
                if mempool.add_transaction(make_signed_tx(&kp, i as u64, *fee, 900)).is_ok() {
                    added += 1;
                }
            }
            prop_assert!(mempool.len() <= added);
        }

        /// Sequential nonces from one sender all land in pending (none queued).
        #[test]
        fn sequential_nonces_all_pending(count in 1usize..15) {
            let kp = Keypair::generate();
            let mut mempool = Mempool::with_defaults();

            for i in 0..count {
                mempool.add_transaction(make_signed_tx(&kp, i as u64, 60_000, 900)).unwrap();
            }
            prop_assert_eq!(mempool.queued_len(), 0);
            prop_assert_eq!(mempool.len(), count);
        }

        /// Nonce gap: submitting nonce 0 then nonce N>1 queues exactly one tx.
        #[test]
        fn nonce_gap_queues_future(gap in 2u64..20) {
            let kp = Keypair::generate();
            let mut mempool = Mempool::with_defaults();

            mempool.add_transaction(make_signed_tx(&kp, 0, 60_000, 900)).unwrap();
            mempool.add_transaction(make_signed_tx(&kp, gap, 60_000, 900)).unwrap();

            prop_assert_eq!(mempool.len(), 2);
            prop_assert_eq!(mempool.queued_len(), 1);
        }

        /// Filling a nonce gap promotes all queued txs for that sender.
        #[test]
        fn filling_gap_promotes_all(gap_start in 2u64..8) {
            let kp = Keypair::generate();
            let mut mempool = Mempool::with_defaults();

            // Add nonce 0
            mempool.add_transaction(make_signed_tx(&kp, 0, 60_000, 900)).unwrap();
            // Add nonces gap_start..gap_start+2 (all queued)
            for n in gap_start..gap_start + 2 {
                mempool.add_transaction(make_signed_tx(&kp, n, 60_000, 900)).unwrap();
            }
            let queued_before = mempool.queued_len();
            prop_assert!(queued_before > 0);

            // Fill the gap: nonces 1..gap_start
            for n in 1..gap_start {
                mempool.add_transaction(make_signed_tx(&kp, n, 60_000, 900)).unwrap();
            }
            prop_assert_eq!(mempool.queued_len(), 0, "all queued txs should be promoted");
        }

        /// get_transactions returns at most max_count items.
        #[test]
        fn get_transactions_respects_max_count(
            n in 1usize..20,
            max_count in 1usize..10,
        ) {
            let mut mempool = Mempool::with_defaults();
            for _ in 0..n {
                let kp = Keypair::generate();
                let _ = mempool.add_transaction(make_signed_tx(&kp, 0, 60_000, 900));
            }
            let txs = mempool.get_transactions(max_count, u64::MAX);
            prop_assert!(txs.len() <= max_count);
        }

        /// get_transactions returns txs sorted by descending fee_rate.
        /// fee_rate = fee / serialized_size (integer division), so txs
        /// with close fees may share the same rate. Ties are broken by
        /// insertion timestamp (FIFO), which is correct pool behaviour.
        #[test]
        fn get_transactions_fee_ordered(
            fees in proptest::collection::vec(50_000u128..500_000, 2..15),
        ) {
            let mut mempool = Mempool::with_defaults();
            // Different sender per tx → nonce 0 each, identical serialized size.
            let mut tx_sizes = Vec::new();
            for fee in fees.iter() {
                let kp = Keypair::generate();
                let tx = make_signed_tx(&kp, 0, *fee, 900);
                let size = bincode::serialize(&tx).unwrap().len() as u128;
                tx_sizes.push(size);
                let _ = mempool.add_transaction(tx);
            }
            let txs = mempool.get_transactions(fees.len(), u64::MAX);
            // Verify fee_rate is non-increasing (the actual ordering key).
            for w in txs.windows(2) {
                let size0 = bincode::serialize(&w[0]).unwrap().len() as u128;
                let size1 = bincode::serialize(&w[1]).unwrap().len() as u128;
                let rate0 = w[0].fee / size0;
                let rate1 = w[1].fee / size1;
                prop_assert!(rate0 >= rate1, "txs should be fee_rate-ordered");
            }
        }

        /// Removing a transaction always decreases pool size by exactly 1.
        #[test]
        fn remove_decreases_size(
            count in 2usize..10,
            remove_idx in 0usize..10,
        ) {
            let mut mempool = Mempool::with_defaults();
            let mut hashes = Vec::new();
            for _ in 0..count {
                let kp = Keypair::generate();
                let tx = make_signed_tx(&kp, 0, 60_000, 900);
                hashes.push(tx.hash());
                let _ = mempool.add_transaction(tx);
            }
            let before = mempool.len();
            let idx = remove_idx % hashes.len();
            mempool.remove_transactions(&[hashes[idx]]);
            prop_assert_eq!(mempool.len(), before - 1);
        }

        /// Nonce-too-low txs are always rejected.
        #[test]
        fn nonce_too_low_rejected(
            chain_nonce in 1u64..100,
            tx_nonce_offset in 1u64..50,
        ) {
            let kp = Keypair::generate();
            let mut mempool = Mempool::with_defaults();
            let sender = PublicKey::from_bytes(kp.public_key().to_vec()).to_address();
            mempool.set_sender_nonce(sender, chain_nonce);

            let low_nonce = chain_nonce.saturating_sub(tx_nonce_offset);
            if low_nonce < chain_nonce {
                let tx = make_signed_tx(&kp, low_nonce, 60_000, 900);
                let result = mempool.add_transaction(tx);
                prop_assert!(result.is_err(), "nonce {} < {} should be rejected", low_nonce, chain_nonce);
            }
        }

        /// Duplicate transactions are always rejected.
        #[test]
        fn duplicate_rejected(fee in 60_000u128..500_000) {
            let kp = Keypair::generate();
            let mut mempool = Mempool::with_defaults();
            let tx = make_signed_tx(&kp, 0, fee, 900);
            mempool.add_transaction(tx.clone()).unwrap();
            let result = mempool.add_transaction(tx);
            prop_assert!(result.is_err(), "duplicate should be rejected");
        }
    }
}
