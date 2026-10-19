use aether_types::Transaction;
use anyhow::Result;
use rayon::prelude::*;
use std::collections::HashSet;

/// Parallel Scheduler for Transaction Execution
///
/// Uses declared R/W sets to partition transactions into non-conflicting
/// batches that can be executed in parallel via rayon.
///
/// Algorithm:
/// 1. Build conflict graph from R/W sets
/// 2. Greedy coloring to partition into independent batches
/// 3. Execute each batch in parallel (batches are sequential)
///
/// Conflict Rule:
/// tx_a conflicts with tx_b if:
/// - W(a) ∩ W(b) ≠ ∅ (write-write)
/// - W(a) ∩ R(b) ≠ ∅ (write-read)
/// - W(b) ∩ R(a) ≠ ∅ (read-write)
/// - Inputs(a) ∩ Inputs(b) ≠ ∅ (UTxO conflict)
pub struct ParallelScheduler {
    max_batch_size: usize,
}

impl ParallelScheduler {
    pub fn new() -> Self {
        ParallelScheduler {
            max_batch_size: 1000,
        }
    }

    /// Partition transactions into non-conflicting batches.
    pub fn schedule(&self, transactions: &[Transaction]) -> Vec<Vec<Transaction>> {
        if transactions.is_empty() {
            return vec![];
        }

        let mut batches: Vec<Vec<Transaction>> = vec![];
        let mut remaining: Vec<Transaction> = transactions.to_vec();

        while !remaining.is_empty() {
            let mut current_batch = vec![];
            let mut used_indices = HashSet::new();

            for (i, tx) in remaining.iter().enumerate() {
                if used_indices.contains(&i) {
                    continue;
                }

                let mut conflicts = false;
                for batch_tx in &current_batch {
                    if tx.conflicts_with(batch_tx) {
                        conflicts = true;
                        break;
                    }
                }

                if !conflicts && !Self::has_pending_dependencies(tx, i, &remaining, &used_indices) {
                    current_batch.push(tx.clone());
                    used_indices.insert(i);

                    if current_batch.len() >= self.max_batch_size {
                        break;
                    }
                }
            }

            remaining = remaining
                .into_iter()
                .enumerate()
                .filter(|(i, _)| !used_indices.contains(i))
                .map(|(_, tx)| tx)
                .collect();

            if !current_batch.is_empty() {
                batches.push(current_batch);
            } else {
                break;
            }
        }

        batches
    }

    /// Check if tx at `idx` depends on an EARLIER unscheduled tx.
    /// This enforces ordering: a tx that reads from an address written by
    /// an earlier tx must wait for the writer to be scheduled first.
    fn has_pending_dependencies(
        tx: &Transaction,
        idx: usize,
        remaining: &[Transaction],
        used_indices: &HashSet<usize>,
    ) -> bool {
        // Read-after-write: this tx reads addr X, an earlier tx writes X
        for addr in &tx.reads {
            for (j, other) in remaining.iter().enumerate() {
                if j >= idx || used_indices.contains(&j) {
                    continue; // Only check EARLIER transactions
                }
                if other.writes.contains(addr) {
                    return true;
                }
            }
        }
        // Write-after-read: this tx writes addr X, an earlier tx reads X
        for addr in &tx.writes {
            for (j, other) in remaining.iter().enumerate() {
                if j >= idx || used_indices.contains(&j) {
                    continue; // Only check EARLIER transactions
                }
                if other.reads.contains(addr) {
                    return true;
                }
            }
        }
        false
    }

    /// Execute batches with rayon parallelism.
    ///
    /// Batches execute sequentially (they have inter-batch dependencies).
    /// Within each batch, transactions execute in parallel via rayon.
    ///
    /// The executor must be `Fn + Sync` (not FnMut) because multiple
    /// threads call it concurrently within a batch.
    pub fn execute_parallel<F>(&self, batches: Vec<Vec<Transaction>>, executor: F) -> Result<()>
    where
        F: Fn(&Transaction) -> Result<()> + Sync,
    {
        for batch in batches {
            if batch.len() == 1 {
                // Single tx — no parallelism overhead
                executor(&batch[0])?;
            } else {
                // Parallel execution within batch
                batch.par_iter().try_for_each(&executor)?;
            }
        }

        Ok(())
    }

    /// Execute batches sequentially (for comparison / fallback).
    pub fn execute_sequential<F>(
        &self,
        batches: Vec<Vec<Transaction>>,
        mut executor: F,
    ) -> Result<()>
    where
        F: FnMut(&Transaction) -> Result<()>,
    {
        for batch in batches {
            for tx in &batch {
                executor(tx)?;
            }
        }
        Ok(())
    }

    /// Execute parallel and collect results per transaction.
    ///
    /// Returns a Vec of results in the same order as the input batches.
    pub fn execute_parallel_collect<F, R>(
        &self,
        batches: Vec<Vec<Transaction>>,
        executor: F,
    ) -> Result<Vec<Vec<R>>>
    where
        F: Fn(&Transaction) -> Result<R> + Sync,
        R: Send,
    {
        let mut all_results = Vec::with_capacity(batches.len());

        for batch in batches {
            let results: Result<Vec<R>> = batch.par_iter().map(&executor).collect();
            all_results.push(results?);
        }

        Ok(all_results)
    }

    /// Calculate potential speedup.
    pub fn speedup_estimate(&self, transactions: &[Transaction]) -> f64 {
        if transactions.is_empty() {
            return 1.0;
        }

        let batches = self.schedule(transactions);
        transactions.len() as f64 / batches.len() as f64
    }
}

impl Default for ParallelScheduler {
    fn default() -> Self {
        Self::new()
    }
}
