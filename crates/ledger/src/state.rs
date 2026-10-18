use aether_crypto_primitives::ed25519;
use aether_state_merkle::SparseMerkleTree;
use aether_state_storage::{
    Storage, StorageBatch, CF_ACCOUNTS, CF_METADATA, CF_SPENT_UTXOS, CF_UTXOS,
};
use aether_types::{
    contract_address, Account, Address, CallPayload, DeployPayload, Transaction,
    TransactionReceipt, TransactionStatus, TransferPayload, Utxo, UtxoId, CALL_PROGRAM_ID,
    DEPLOY_PROGRAM_ID, H256, TRANSFER_PROGRAM_ID,
};
use anyhow::{anyhow, bail, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// Metadata key prefix for contract code (`code:` || code_hash).
const CONTRACT_CODE_PREFIX: &[u8] = b"code:";
/// Metadata key prefix for contract storage (`cstore:` || address).
const CONTRACT_STORAGE_PREFIX: &[u8] = b"cstore:";
/// Maximum deployable contract size.
pub const MAX_CONTRACT_CODE_BYTES: usize = 64 * 1024;

/// Block-level context passed to contract execution. Every validator derives
/// these values from the block header, so execution stays deterministic.
#[derive(Clone, Copy, Debug, Default)]
pub struct BlockContext {
    pub slot: u64,
    pub timestamp: u64,
}

/// A single contract invocation handed to a [`ContractExecutor`].
pub struct ContractCall<'a> {
    pub code: &'a [u8],
    pub code_hash: H256,
    pub contract: Address,
    pub caller: Address,
    pub input: &'a [u8],
    pub gas_limit: u64,
    pub context: BlockContext,
    /// Full current storage of the contract.
    pub storage: HashMap<Vec<u8>, Vec<u8>>,
}

/// Result of a contract invocation.
pub struct ContractOutcome {
    pub success: bool,
    pub gas_used: u64,
    /// Full storage of the contract after execution.
    pub storage: HashMap<Vec<u8>, Vec<u8>>,
    pub logs: Vec<Vec<u8>>,
    pub return_data: Vec<u8>,
}

/// Executes contract code. The ledger is runtime-agnostic; the node plugs in
/// the wasmtime-based VM from `aether-runtime`.
pub trait ContractExecutor: Send + Sync {
    /// Check that `code` is a loadable module (deterministic accept/reject).
    fn validate(&self, code: &[u8]) -> Result<()>;
    fn execute(&self, call: ContractCall<'_>) -> Result<ContractOutcome>;
}

/// State changes produced by a deploy or call, applied only if the whole
/// transaction validates.
struct ContractEffects {
    account: Account,
    code: Option<(H256, Vec<u8>)>,
    storage_bytes: Vec<u8>,
    logs: Vec<Vec<u8>>,
    gas_used: u64,
}

fn sha256_h256(bytes: &[u8]) -> H256 {
    use sha2::{Digest, Sha256};
    H256(Sha256::digest(bytes).into())
}

fn contract_code_key(code_hash: &H256) -> Vec<u8> {
    let mut key = CONTRACT_CODE_PREFIX.to_vec();
    key.extend_from_slice(code_hash.as_bytes());
    key
}

fn contract_storage_key(address: &Address) -> Vec<u8> {
    let mut key = CONTRACT_STORAGE_PREFIX.to_vec();
    key.extend_from_slice(address.as_bytes());
    key
}

/// In-memory overlay for speculative block execution.
/// Reads check overlay first, falls back to storage. Writes stay in memory
/// until explicitly committed via `commit_overlay()`.
///
/// Overlays can be chained: a block executed on top of a not-yet-finalized
/// parent gets an overlay whose `parent` is the parent block's overlay, so
/// reads see every pending ancestor before falling back to (finalized)
/// storage. `writes`/`deletes` only hold this block's own delta.
#[derive(Debug)]
pub struct PendingOverlay {
    pub writes: HashMap<(String, Vec<u8>), Vec<u8>>,
    pub deletes: HashSet<(String, Vec<u8>)>,
    pub changed_accounts: Vec<Address>,
    pub state_root: H256,
    parent: RwLock<Option<Arc<PendingOverlay>>>,
}

impl PendingOverlay {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_parent(None)
    }

    /// Create an empty overlay layered on top of `parent`.
    pub fn with_parent(parent: Option<Arc<PendingOverlay>>) -> Self {
        PendingOverlay {
            writes: HashMap::new(),
            deletes: HashSet::new(),
            changed_accounts: Vec::new(),
            state_root: H256::zero(),
            parent: RwLock::new(parent),
        }
    }

    /// Drop the link to the parent overlay. Called once this overlay's
    /// ancestors have been written to storage, so reads fall through to disk
    /// and finalized overlays can be freed.
    pub fn detach_parent(&self) {
        if let Ok(mut guard) = self.parent.write() {
            *guard = None;
        }
    }

    fn put(&mut self, cf: &str, key: Vec<u8>, value: Vec<u8>) {
        self.deletes.remove(&(cf.to_string(), key.clone()));
        self.writes.insert((cf.to_string(), key), value);
    }

    fn delete(&mut self, cf: &str, key: Vec<u8>) {
        self.writes.remove(&(cf.to_string(), key.clone()));
        self.deletes.insert((cf.to_string(), key));
    }

    /// Look up `key` in this overlay and then its pending ancestors.
    /// `None` means "not touched by any pending block, read storage".
    pub fn get(&self, cf: &str, key: &[u8]) -> Option<Option<Vec<u8>>> {
        let map_key = (cf.to_string(), key.to_vec());
        if self.deletes.contains(&map_key) {
            return Some(None); // Deleted in overlay
        }
        if let Some(v) = self.writes.get(&map_key) {
            return Some(Some(v.clone()));
        }
        let mut next = self.parent.read().ok().and_then(|p| p.clone());
        while let Some(layer) = next {
            if layer.deletes.contains(&map_key) {
                return Some(None);
            }
            if let Some(v) = layer.writes.get(&map_key) {
                return Some(Some(v.clone()));
            }
            next = layer.parent.read().ok().and_then(|p| p.clone());
        }
        None
    }
}

pub struct Ledger {
    storage: Storage,
    merkle_tree: SparseMerkleTree,
    executor: Option<Arc<dyn ContractExecutor>>,
}

impl Ledger {
    pub fn new(storage: Storage) -> Result<Self> {
        let mut ledger = Ledger {
            storage,
            merkle_tree: SparseMerkleTree::new(),
            executor: None,
        };

        ledger.load_state_root()?;
        Ok(ledger)
    }

    fn load_state_root(&mut self) -> Result<()> {
        // Always rebuild Merkle tree from accounts on startup.
        // This handles both normal restart (metadata exists) and recovery
        // (metadata missing but accounts exist in storage).
        let has_metadata = self.storage.get(CF_METADATA, b"state_root")?.is_some();
        let has_accounts = self.storage.iterator(CF_ACCOUNTS)?.next().is_some();

        if has_metadata || has_accounts {
            self.recompute_state_root()?;
        }
        Ok(())
    }

    pub fn state_root(&self) -> H256 {
        self.merkle_tree.root()
    }

    /// Install the contract runtime. Without one, deploy/call transactions fail.
    pub fn set_contract_executor(&mut self, executor: Arc<dyn ContractExecutor>) {
        self.executor = Some(executor);
    }

    /// The Merkle tree of the finalized (on-disk) state.
    pub fn merkle_tree(&self) -> &SparseMerkleTree {
        &self.merkle_tree
    }

    /// Read an account as seen by a pending overlay (or finalized storage).
    pub fn account_in(
        &self,
        overlay: Option<&PendingOverlay>,
        address: &Address,
    ) -> Result<Account> {
        match overlay {
            Some(o) => self.get_account_from_overlay(o, address),
            None => self.get_or_create_account(address),
        }
    }

    /// Read a contract's storage map as seen by a pending overlay.
    pub fn contract_storage_in(
        &self,
        overlay: Option<&PendingOverlay>,
        address: &Address,
    ) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        let key = contract_storage_key(address);
        let raw = match overlay {
            Some(o) => self.read_raw(o, CF_METADATA, &key)?,
            None => self.storage.get(CF_METADATA, &key)?,
        };
        match raw {
            Some(bytes) => Ok(bincode::deserialize(&bytes)?),
            None => Ok(BTreeMap::new()),
        }
    }

    /// Read a contract's storage map from finalized state.
    pub fn get_contract_storage(&self, address: &Address) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        match self
            .storage
            .get(CF_METADATA, &contract_storage_key(address))?
        {
            Some(bytes) => Ok(bincode::deserialize(&bytes)?),
            None => Ok(BTreeMap::new()),
        }
    }

    /// Read `key` from (overlay, then storage).
    fn read_raw(&self, overlay: &PendingOverlay, cf: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match overlay.get(cf, key) {
            Some(v) => Ok(v),
            None => Ok(self.storage.get(cf, key)?),
        }
    }

    /// Access the underlying storage (for block/receipt persistence).
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    pub fn get_account(&self, address: &Address) -> Result<Option<Account>> {
        match self.storage.get(CF_ACCOUNTS, address.as_bytes())? {
            Some(bytes) => {
                let account: Account = bincode::deserialize(&bytes)?;
                Ok(Some(account))
            }
            None => Ok(None),
        }
    }

    pub fn get_or_create_account(&self, address: &Address) -> Result<Account> {
        match self.get_account(address)? {
            Some(account) => Ok(account),
            None => Ok(Account::new(*address)),
        }
    }

    pub fn get_utxo(&self, utxo_id: &UtxoId) -> Result<Option<Utxo>> {
        let key = bincode::serialize(utxo_id)?;
        match self.storage.get(CF_UTXOS, &key)? {
            Some(bytes) => {
                let utxo: Utxo = bincode::deserialize(&bytes)?;
                Ok(Some(utxo))
            }
            None => Ok(None),
        }
    }

    pub fn apply_transaction(&mut self, tx: &Transaction) -> Result<TransactionReceipt> {
        let _span = tracing::debug_span!("apply_transaction", tx_hash = ?tx.hash()).entered();
        tx.verify_signature()?;
        self.apply_transaction_validated(tx)
    }

    fn apply_transaction_validated(&mut self, tx: &Transaction) -> Result<TransactionReceipt> {
        // Reject duplicate UTxO inputs within a single transaction.
        // Without this, an attacker lists the same input twice, counting its value
        // double and effectively minting tokens out of thin air.
        {
            let mut seen_inputs = HashSet::new();
            for input in &tx.inputs {
                if !seen_inputs.insert(input) {
                    bail!("duplicate UTxO input in transaction: {:?}", input);
                }
            }
        }

        // Validate UTxO inputs: existence, ownership, and accumulate total in one pass
        let mut total_input = 0u128;
        for input in &tx.inputs {
            let utxo = self
                .get_utxo(input)?
                .ok_or_else(|| anyhow!("UTxO input not found: {:?}", input))?;
            if utxo.owner != tx.sender {
                bail!(
                    "UTxO input {:?} owned by {:?}, not sender {:?}",
                    input,
                    utxo.owner,
                    tx.sender
                );
            }
            total_input = total_input
                .checked_add(utxo.amount)
                .ok_or_else(|| anyhow!("UTxO total input overflow"))?;
        }

        let transfer_payload = self.decode_transfer_payload(tx)?;
        if transfer_payload.is_some() && (!tx.inputs.is_empty() || !tx.outputs.is_empty()) {
            bail!("transfer program transactions cannot mix UTxO inputs/outputs");
        }

        // Validate sender account
        let mut sender_account = self.get_or_create_account(&tx.sender)?;
        if sender_account.nonce != tx.nonce {
            bail!(
                "invalid nonce: expected {}, got {}",
                sender_account.nonce,
                tx.nonce
            );
        }

        let is_utxo_tx = !tx.inputs.is_empty() || !tx.outputs.is_empty();

        // For UTxO transactions, the fee is paid from the UTxO surplus
        // (total_input - total_output >= fee). For account/transfer transactions,
        // the fee is deducted from the sender's account balance.
        if !is_utxo_tx {
            let transfer_amount = transfer_payload.as_ref().map(|p| p.amount).unwrap_or(0);
            let total_debit = tx
                .fee
                .checked_add(transfer_amount)
                .ok_or_else(|| anyhow!("fee + transfer amount overflow"))?;
            if sender_account.balance < total_debit {
                bail!("insufficient balance for fee and transfer amount");
            }
            sender_account.balance = sender_account
                .balance
                .checked_sub(total_debit)
                .ok_or_else(|| anyhow!("balance underflow during debit"))?;
        }

        sender_account.nonce = sender_account
            .nonce
            .checked_add(1)
            .ok_or_else(|| anyhow!("nonce overflow"))?;

        // Track which accounts changed for incremental Merkle update
        let mut changed_accounts = vec![tx.sender];

        let mut recipient_account: Option<Account> = None;
        if let Some(payload) = &transfer_payload {
            if payload.recipient == tx.sender {
                sender_account.balance = sender_account
                    .balance
                    .checked_add(payload.amount)
                    .ok_or_else(|| anyhow!("sender balance overflow"))?;
            } else {
                let mut recipient = self.get_or_create_account(&payload.recipient)?;
                recipient.balance = recipient
                    .balance
                    .checked_add(payload.amount)
                    .ok_or_else(|| anyhow!("recipient balance overflow"))?;
                changed_accounts.push(payload.recipient);
                recipient_account = Some(recipient);
            }
        }

        // Create new UTxOs (outputs)
        let mut total_output = 0u128;
        for output in &tx.outputs {
            total_output = total_output
                .checked_add(output.amount)
                .ok_or_else(|| anyhow!("UTxO total output overflow"))?;
        }

        // Validate UTxO balance: inputs must cover outputs + fee
        if is_utxo_tx {
            let required = total_output
                .checked_add(tx.fee)
                .ok_or_else(|| anyhow!("UTxO output + fee overflow"))?;
            if total_input < required {
                bail!("UTxO inputs insufficient for outputs + fee");
            }
        } else if total_input < total_output {
            bail!("UTxO inputs insufficient for outputs");
        }

        // Apply changes
        let mut batch = StorageBatch::new();

        // Update sender account
        self.update_account_in_batch(&mut batch, sender_account.clone())?;
        if let Some(ref account) = recipient_account {
            self.update_account_in_batch(&mut batch, account.clone())?;
        }

        // Delete consumed UTxOs
        for input in &tx.inputs {
            let key = bincode::serialize(input)?;
            batch.delete(CF_UTXOS, key);
        }

        // Create new UTxOs
        let tx_hash = tx.hash();
        for (idx, output) in tx.outputs.iter().enumerate() {
            let output_index: u32 = u32::try_from(idx)
                .map_err(|_| anyhow!("transaction output index exceeds u32::MAX"))?;
            let utxo_id = UtxoId {
                tx_hash,
                output_index,
            };
            let utxo = Utxo {
                amount: output.amount,
                owner: output.owner.to_address(),
                script_hash: output.script_hash,
            };
            let key = bincode::serialize(&utxo_id)?;
            let value = bincode::serialize(&utxo)?;
            batch.put(CF_UTXOS, key, value);
        }

        // Incremental Merkle update — include state_root in the same batch
        self.update_state_root_incremental(
            &sender_account,
            recipient_account.as_ref(),
            Some(&mut batch),
        )?;

        // Commit everything atomically in a single WriteBatch
        self.storage.write_batch(batch)?;

        Ok(TransactionReceipt {
            tx_hash,
            block_hash: H256::zero(), // Set by block processor
            slot: 0,                  // Set by block processor
            status: TransactionStatus::Success,
            gas_used: 0, // Would be computed by runtime
            logs: vec![],
            state_root: self.state_root(),
        })
    }

    fn decode_transfer_payload(&self, tx: &Transaction) -> Result<Option<TransferPayload>> {
        if tx.program_id != Some(TRANSFER_PROGRAM_ID) {
            return Ok(None);
        }
        if tx.data.is_empty() {
            bail!("transfer program payload is empty");
        }

        let payload: TransferPayload = bincode::deserialize(&tx.data)
            .map_err(|e| anyhow!("invalid transfer payload encoding: {e}"))?;
        if payload.amount == 0 {
            bail!("transfer amount must be greater than zero");
        }

        Ok(Some(payload))
    }

    fn update_account_in_batch(&self, batch: &mut StorageBatch, account: Account) -> Result<()> {
        let key = account.address.as_bytes().to_vec();
        let value = bincode::serialize(&account)?;
        batch.put(CF_ACCOUNTS, key, value);
        Ok(())
    }

    /// Incrementally update the Merkle tree for changed accounts only.
    /// This is O(k) where k = number of changed accounts, instead of O(n) for all accounts.
    ///
    /// If `batch` is provided, the state root is written into the batch for atomic
    /// commit with other state changes. Otherwise, the root is written directly.
    fn update_state_root_incremental(
        &mut self,
        sender: &Account,
        recipient: Option<&Account>,
        batch: Option<&mut StorageBatch>,
    ) -> Result<()> {
        // Update sender leaf
        let sender_hash = self.hash_account(sender);
        self.merkle_tree.update(sender.address, sender_hash);

        // Update recipient leaf (if different from sender)
        if let Some(recipient) = recipient {
            let recipient_hash = self.hash_account(recipient);
            self.merkle_tree.update(recipient.address, recipient_hash);
        }

        // Persist the new root — either in the batch (atomic) or directly
        let root = self.merkle_tree.root();
        if let Some(batch) = batch {
            batch.put(
                CF_METADATA,
                b"state_root".to_vec(),
                root.as_bytes().to_vec(),
            );
        } else {
            self.storage
                .put(CF_METADATA, b"state_root", root.as_bytes())?;
        }

        Ok(())
    }

    /// Full rebuild of state root from all accounts (used on startup/recovery).
    /// Safe because this runs during initialization before any concurrent access.
    fn recompute_state_root(&mut self) -> Result<()> {
        let mut accounts = HashMap::new();
        for item in self.storage.iterator(CF_ACCOUNTS)? {
            let (key_bytes, value_bytes) = item;
            if key_bytes.len() == 20 {
                let address = Address::from_slice(&key_bytes).map_err(|e| anyhow!(e))?;
                let account: Account = bincode::deserialize(&value_bytes)?;
                let account_hash = self.hash_account(&account);
                accounts.insert(address, account_hash);
            }
        }

        self.merkle_tree = SparseMerkleTree::new();
        for (address, hash) in accounts {
            self.merkle_tree.update(address, hash);
        }

        // Persist recomputed root atomically
        let root = self.merkle_tree.root();
        let mut batch = StorageBatch::new();
        batch.put(
            CF_METADATA,
            b"state_root".to_vec(),
            root.as_bytes().to_vec(),
        );
        self.storage.write_batch(batch)?;

        Ok(())
    }

    fn hash_account(&self, account: &Account) -> H256 {
        use sha2::{Digest, Sha256};
        let bytes = match bincode::serialize(account) {
            Ok(b) => b,
            Err(_) => return H256::zero(),
        };
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        H256(hash)
    }

    /// Record burned fees in ledger metadata. This permanently removes tokens from
    /// circulation, implementing EIP-1559 deflationary pressure.
    pub fn record_burned_fees(&mut self, amount: u128) -> Result<()> {
        if amount == 0 {
            return Ok(());
        }
        let current = self
            .storage
            .get(CF_METADATA, b"total_burned")?
            .map(|bytes| {
                let mut arr = [0u8; 16];
                arr.copy_from_slice(&bytes[..16.min(bytes.len())]);
                u128::from_le_bytes(arr)
            })
            .unwrap_or(0);
        let new_total = current
            .checked_add(amount)
            .ok_or_else(|| anyhow!("total_burned overflow"))?;
        self.storage
            .put(CF_METADATA, b"total_burned", &new_total.to_le_bytes())?;
        Ok(())
    }

    /// Fold proposer reward and burn accounting into an existing write batch,
    /// making fee distribution atomic with the overlay commit.
    ///
    /// Must be called AFTER `prepare_overlay_batch` (so the in-memory Merkle tree
    /// already reflects the overlay state) and BEFORE the batch is written to disk.
    ///
    /// Uses `overlay` to read the proposer's post-transaction balance in case the
    /// proposer also submitted transactions in this block.
    pub fn fold_fee_distribution_into_batch(
        &mut self,
        batch: &mut StorageBatch,
        overlay: &PendingOverlay,
        proposer: &Address,
        proposer_reward: u128,
        burned: u128,
        treasury_fee: u128,
    ) -> Result<()> {
        if proposer_reward > 0 {
            // Read proposer account from overlay first (in case proposer submitted txs),
            // falling back to DB if not present in the overlay.
            let mut account = self.get_account_from_overlay(overlay, proposer)?;
            account.balance = account
                .balance
                .checked_add(proposer_reward)
                .ok_or_else(|| anyhow!("proposer balance overflow"))?;
            self.update_account_in_batch(batch, account.clone())?;
            // Overwrites the state_root entry already in the batch with the updated root.
            self.update_state_root_incremental(&account, None, Some(batch))?;
        }

        if burned > 0 {
            let current = self
                .storage
                .get(CF_METADATA, b"total_burned")?
                .map(|bytes| {
                    let mut arr = [0u8; 16];
                    arr.copy_from_slice(&bytes[..16.min(bytes.len())]);
                    u128::from_le_bytes(arr)
                })
                .unwrap_or(0);
            let new_total = current
                .checked_add(burned)
                .ok_or_else(|| anyhow!("total_burned overflow in batch"))?;
            batch.put(
                CF_METADATA,
                b"total_burned".to_vec(),
                new_total.to_le_bytes().to_vec(),
            );
        }

        // Accumulate treasury fees in metadata. These are held in reserve
        // until the governance module is wired in and can claim them.
        if treasury_fee > 0 {
            let current = self
                .storage
                .get(CF_METADATA, b"total_treasury_fees")?
                .map(|bytes| {
                    let mut arr = [0u8; 16];
                    arr.copy_from_slice(&bytes[..16.min(bytes.len())]);
                    u128::from_le_bytes(arr)
                })
                .unwrap_or(0);
            let new_total = current
                .checked_add(treasury_fee)
                .ok_or_else(|| anyhow!("total_treasury_fees overflow in batch"))?;
            batch.put(
                CF_METADATA,
                b"total_treasury_fees".to_vec(),
                new_total.to_le_bytes().to_vec(),
            );
        }

        Ok(())
    }

    /// Get the total amount of fees burned since genesis.
    pub fn total_burned(&self) -> u128 {
        self.storage
            .get(CF_METADATA, b"total_burned")
            .ok()
            .flatten()
            .map(|bytes| {
                let mut arr = [0u8; 16];
                arr.copy_from_slice(&bytes[..16.min(bytes.len())]);
                u128::from_le_bytes(arr)
            })
            .unwrap_or(0)
    }

    /// Get the total treasury fees accumulated since genesis.
    /// These fees are held in reserve until governance can claim them.
    pub fn total_treasury_fees(&self) -> u128 {
        self.storage
            .get(CF_METADATA, b"total_treasury_fees")
            .ok()
            .flatten()
            .map(|bytes| {
                let mut arr = [0u8; 16];
                arr.copy_from_slice(&bytes[..16.min(bytes.len())]);
                u128::from_le_bytes(arr)
            })
            .unwrap_or(0)
    }

    /// Credit an account with a reward (for epoch emissions, proposer rewards).
    /// This writes immediately — prefer `credit_account_to_batch` when multiple
    /// credits should be committed atomically.
    pub fn credit_account(&mut self, address: &Address, amount: u128) -> Result<()> {
        let mut batch = StorageBatch::new();
        self.credit_account_to_batch(&mut batch, address, amount)?;
        self.storage.write_batch(batch)?;
        Ok(())
    }

    /// Accumulate an account credit into an existing `StorageBatch`.
    ///
    /// Use this when multiple credits (e.g. emission rewards + unbonding returns)
    /// must be committed atomically in a single WriteBatch. The caller is
    /// responsible for calling `write_batch` on the resulting batch.
    pub fn credit_account_to_batch(
        &mut self,
        batch: &mut StorageBatch,
        address: &Address,
        amount: u128,
    ) -> Result<()> {
        let mut account = self.get_or_create_account(address)?;
        account.balance = account
            .balance
            .checked_add(amount)
            .ok_or_else(|| anyhow!("balance overflow crediting account"))?;

        self.update_account_in_batch(batch, account.clone())?;
        self.update_state_root_incremental(&account, None, Some(batch))?;
        Ok(())
    }

    /// Save a snapshot of the current state root for potential rollback.
    pub fn snapshot_state_root(&self) -> H256 {
        self.merkle_tree.root()
    }

    /// Execute a block's transactions speculatively without committing to storage.
    /// Returns receipts, the computed state root, and a PendingOverlay that can be
    /// committed or discarded.
    pub fn apply_block_speculatively(
        &mut self,
        transactions: &[Transaction],
    ) -> Result<(Vec<TransactionReceipt>, PendingOverlay)> {
        self.apply_block_speculatively_with_chain_id(transactions, None)
    }

    /// Execute a block's transactions speculatively, optionally validating chain_id.
    pub fn apply_block_speculatively_with_chain_id(
        &mut self,
        transactions: &[Transaction],
        expected_chain_id: Option<u64>,
    ) -> Result<(Vec<TransactionReceipt>, PendingOverlay)> {
        let base_tree = self.merkle_tree.clone();
        let (receipts, overlay, _tree) = self.apply_block_on(
            None,
            base_tree,
            transactions,
            expected_chain_id,
            BlockContext::default(),
        )?;
        Ok((receipts, overlay))
    }

    /// Execute `transactions` on top of a pending parent state.
    ///
    /// `parent` is the parent block's overlay (or `None` when the parent is the
    /// finalized on-disk state) and `base_tree` is the Merkle tree after the
    /// parent. Nothing is written to storage. Returns the receipts, this
    /// block's delta overlay (chained to `parent`) and the post-block tree.
    ///
    /// Every receipt's `state_root` is the post-block state root: the tree is
    /// recomputed once per block instead of once per transaction.
    pub fn apply_block_on(
        &self,
        parent: Option<Arc<PendingOverlay>>,
        base_tree: SparseMerkleTree,
        transactions: &[Transaction],
        expected_chain_id: Option<u64>,
        context: BlockContext,
    ) -> Result<(Vec<TransactionReceipt>, PendingOverlay, SparseMerkleTree)> {
        let _span = tracing::info_span!("apply_block_speculative", tx_count = transactions.len(),)
            .entered();
        let start = Instant::now();
        let mut overlay = PendingOverlay::with_parent(parent);
        let mut spec_tree = base_tree;
        let mut receipts = Vec::with_capacity(transactions.len());

        if transactions.is_empty() {
            spec_tree.recompute();
            overlay.state_root = spec_tree.root();
            return Ok((receipts, overlay, spec_tree));
        }

        for tx in transactions {
            let derived = tx.sender_pubkey.to_address();
            if derived != tx.sender {
                bail!(
                    "sender address does not match public key: tx_hash={}",
                    tx.hash()
                );
            }
        }

        let batch_inputs: Vec<_> = transactions.iter().map(|tx| tx.ed25519_tuple()).collect();
        let batch_results = ed25519::verify_batch(&batch_inputs)
            .map_err(|e| anyhow!("batch signature verification failed: {e:?}"))?;

        for (tx, is_valid) in transactions.iter().zip(batch_results.iter()) {
            if !*is_valid {
                bail!(
                    "block contains transaction with invalid signature: tx_hash={}",
                    tx.hash()
                );
            }
        }

        for tx in transactions {
            // Validate chain_id to prevent cross-chain replay attacks
            if let Some(expected_id) = expected_chain_id {
                if tx.chain_id != expected_id {
                    receipts.push(TransactionReceipt {
                        tx_hash: tx.hash(),
                        block_hash: H256::zero(),
                        slot: 0,
                        status: TransactionStatus::Failed {
                            reason: format!(
                                "wrong chain_id: expected {}, got {}",
                                expected_id, tx.chain_id
                            ),
                        },
                        gas_used: 0,
                        logs: vec![],
                        state_root: H256::zero(),
                    });
                    continue;
                }
            }

            match self.apply_tx_to_overlay(tx, &mut overlay, &mut spec_tree, &context) {
                Ok(receipt) => receipts.push(receipt),
                Err(e) => {
                    receipts.push(TransactionReceipt {
                        tx_hash: tx.hash(),
                        block_hash: H256::zero(),
                        slot: 0,
                        status: TransactionStatus::Failed {
                            reason: e.to_string(),
                        },
                        gas_used: 0,
                        logs: vec![],
                        state_root: H256::zero(),
                    });
                }
            }
        }

        spec_tree.recompute();
        let root = spec_tree.root();
        for receipt in &mut receipts {
            receipt.state_root = root;
        }
        overlay.state_root = root;
        tracing::info!(
            elapsed_us = start.elapsed().as_micros() as u64,
            tx_count = transactions.len(),
            "Speculative block execution complete"
        );
        Ok((receipts, overlay, spec_tree))
    }

    /// Credit the proposer reward and accumulate burn/treasury totals inside
    /// a pending overlay (so descendants of this block observe them and the
    /// result is identical on every validator). Updates `tree` lazily.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_fee_distribution(
        &self,
        overlay: &mut PendingOverlay,
        tree: &mut SparseMerkleTree,
        proposer: &Address,
        proposer_reward: u128,
        burned: u128,
        treasury_fee: u128,
    ) -> Result<()> {
        if proposer_reward > 0 {
            let mut account = self.get_account_from_overlay(overlay, proposer)?;
            account.balance = account
                .balance
                .checked_add(proposer_reward)
                .ok_or_else(|| anyhow!("proposer balance overflow"))?;
            overlay.put(
                CF_ACCOUNTS,
                proposer.as_bytes().to_vec(),
                bincode::serialize(&account)?,
            );
            overlay.changed_accounts.push(*proposer);
            tree.set_leaf(*proposer, self.hash_account(&account));
        }
        for (key, amount) in [
            (&b"total_burned"[..], burned),
            (&b"total_treasury_fees"[..], treasury_fee),
        ] {
            if amount == 0 {
                continue;
            }
            let current = self
                .read_raw(overlay, CF_METADATA, key)?
                .map(|bytes| {
                    let mut arr = [0u8; 16];
                    let n = 16.min(bytes.len());
                    arr[..n].copy_from_slice(&bytes[..n]);
                    u128::from_le_bytes(arr)
                })
                .unwrap_or(0);
            let new_total = current
                .checked_add(amount)
                .ok_or_else(|| anyhow!("fee accounting overflow"))?;
            overlay.put(CF_METADATA, key.to_vec(), new_total.to_le_bytes().to_vec());
        }
        tree.recompute();
        overlay.state_root = tree.root();
        Ok(())
    }

    /// Write one pending block's own delta (plus `extra` block/receipt data)
    /// to storage atomically, and adopt `tree` as the finalized state tree.
    ///
    /// Pending blocks must be committed parent-first.
    pub fn commit_pending(
        &mut self,
        overlay: &PendingOverlay,
        tree: &SparseMerkleTree,
        extra: StorageBatch,
    ) -> Result<()> {
        let mut batch = StorageBatch::new();
        for ((cf, key), value) in &overlay.writes {
            batch.put(cf, key.clone(), value.clone());
        }
        for (cf, key) in &overlay.deletes {
            batch.delete(cf, key.clone());
        }
        let mut tree = tree.clone();
        tree.recompute();
        batch.put(
            CF_METADATA,
            b"state_root".to_vec(),
            tree.root().as_bytes().to_vec(),
        );
        batch.extend(extra);
        self.storage.write_batch(batch)?;
        self.merkle_tree = tree;
        Ok(())
    }

    /// Run a deploy or call transaction. Returns `None` for other programs.
    fn execute_contract(
        &self,
        tx: &Transaction,
        overlay: &PendingOverlay,
        context: &BlockContext,
    ) -> Result<Option<ContractEffects>> {
        let program = match tx.program_id {
            Some(p) if p == DEPLOY_PROGRAM_ID || p == CALL_PROGRAM_ID => p,
            _ => return Ok(None),
        };
        let executor = self
            .executor
            .as_ref()
            .ok_or_else(|| anyhow!("contract runtime not available on this node"))?;

        if program == DEPLOY_PROGRAM_ID {
            let payload: DeployPayload = bincode::deserialize(&tx.data)
                .map_err(|e| anyhow!("invalid deploy payload: {e}"))?;
            if payload.code.is_empty() || payload.code.len() > MAX_CONTRACT_CODE_BYTES {
                bail!(
                    "contract code must be 1..={} bytes, got {}",
                    MAX_CONTRACT_CODE_BYTES,
                    payload.code.len()
                );
            }
            executor.validate(&payload.code)?;
            let address = contract_address(&tx.sender, tx.nonce);
            let existing = self.get_account_from_overlay(overlay, &address)?;
            if existing.code_hash.is_some() {
                bail!("contract already deployed at {:?}", address);
            }
            let code_hash = sha256_h256(&payload.code);
            let storage: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
            let storage_bytes = bincode::serialize(&storage)?;
            let mut account = existing;
            account.code_hash = Some(code_hash);
            account.storage_root = sha256_h256(&storage_bytes);
            return Ok(Some(ContractEffects {
                account,
                code: Some((code_hash, payload.code)),
                storage_bytes,
                logs: vec![],
                gas_used: 0,
            }));
        }

        let payload: CallPayload =
            bincode::deserialize(&tx.data).map_err(|e| anyhow!("invalid call payload: {e}"))?;
        let account = self.get_account_from_overlay(overlay, &payload.contract)?;
        let code_hash = account
            .code_hash
            .ok_or_else(|| anyhow!("no contract at {:?}", payload.contract))?;
        let code = self
            .read_raw(overlay, CF_METADATA, &contract_code_key(&code_hash))?
            .ok_or_else(|| anyhow!("contract code missing for {:?}", payload.contract))?;
        let current: BTreeMap<Vec<u8>, Vec<u8>> = match self.read_raw(
            overlay,
            CF_METADATA,
            &contract_storage_key(&payload.contract),
        )? {
            Some(bytes) => bincode::deserialize(&bytes)?,
            None => BTreeMap::new(),
        };
        let outcome = executor.execute(ContractCall {
            code: &code,
            code_hash,
            contract: payload.contract,
            caller: tx.sender,
            input: &payload.input,
            gas_limit: tx.gas_limit,
            context: *context,
            storage: current.into_iter().collect(),
        })?;
        if !outcome.success {
            bail!("contract execution failed (gas used {})", outcome.gas_used);
        }
        let storage: BTreeMap<Vec<u8>, Vec<u8>> = outcome.storage.into_iter().collect();
        let storage_bytes = bincode::serialize(&storage)?;
        let mut account = account;
        account.storage_root = sha256_h256(&storage_bytes);
        Ok(Some(ContractEffects {
            account,
            code: None,
            storage_bytes,
            logs: outcome.logs,
            gas_used: outcome.gas_used,
        }))
    }

    /// Apply a single transaction to the overlay (not to disk).
    fn apply_tx_to_overlay(
        &self,
        tx: &Transaction,
        overlay: &mut PendingOverlay,
        spec_tree: &mut SparseMerkleTree,
        context: &BlockContext,
    ) -> Result<TransactionReceipt> {
        // Read sender account from overlay first, then storage
        let mut sender_account = self.get_account_from_overlay(overlay, &tx.sender)?;
        if sender_account.nonce != tx.nonce {
            bail!(
                "invalid nonce: expected {}, got {}",
                sender_account.nonce,
                tx.nonce
            );
        }

        let transfer_payload = self.decode_transfer_payload(tx)?;
        if transfer_payload.is_some() && (!tx.inputs.is_empty() || !tx.outputs.is_empty()) {
            bail!("transfer program transactions cannot mix UTxO inputs/outputs");
        }

        let is_utxo_tx = !tx.inputs.is_empty() || !tx.outputs.is_empty();

        // For UTxO transactions, the fee is paid from the UTxO surplus
        // (total_input - total_output >= fee). For account/transfer transactions,
        // the fee is deducted from the sender's account balance.
        if !is_utxo_tx {
            let transfer_amount = transfer_payload.as_ref().map(|p| p.amount).unwrap_or(0);
            let total_debit = tx
                .fee
                .checked_add(transfer_amount)
                .ok_or_else(|| anyhow!("fee + transfer amount overflow"))?;
            if sender_account.balance < total_debit {
                bail!("insufficient balance for fee and transfer amount");
            }
            sender_account.balance = sender_account
                .balance
                .checked_sub(total_debit)
                .ok_or_else(|| anyhow!("balance underflow during debit"))?;
        }

        sender_account.nonce = sender_account
            .nonce
            .checked_add(1)
            .ok_or_else(|| anyhow!("nonce overflow"))?;

        let mut recipient_account: Option<Account> = None;
        if let Some(payload) = &transfer_payload {
            if payload.recipient == tx.sender {
                sender_account.balance = sender_account
                    .balance
                    .checked_add(payload.amount)
                    .ok_or_else(|| anyhow!("sender balance overflow"))?;
            } else {
                let mut recipient = self.get_account_from_overlay(overlay, &payload.recipient)?;
                recipient.balance = recipient
                    .balance
                    .checked_add(payload.amount)
                    .ok_or_else(|| anyhow!("recipient balance overflow"))?;
                recipient_account = Some(recipient);
            }
        }

        // --- All validation BEFORE any overlay writes ---
        // This ensures a failing transaction does not corrupt the overlay
        // (e.g., nonce increment visible to later transactions in the block).

        // Reject duplicate UTxO inputs within a single transaction.
        // Without this check, an attacker could list the same input twice,
        // counting its value double and effectively minting tokens.
        {
            let mut seen_inputs = HashSet::new();
            for input in &tx.inputs {
                if !seen_inputs.insert(input) {
                    bail!("duplicate UTxO input in transaction: {:?}", input);
                }
            }
        }

        // Validate UTxO inputs: existence, ownership, and accumulate total
        let mut total_input = 0u128;
        for input in &tx.inputs {
            let key = bincode::serialize(input)?;
            let utxo: Utxo = match overlay.get(CF_UTXOS, &key) {
                Some(Some(bytes)) => bincode::deserialize(&bytes)?,
                Some(None) => bail!("UTxO input already spent in this block: {:?}", input),
                None => {
                    let stored = self
                        .get_utxo(input)?
                        .ok_or_else(|| anyhow!("UTxO input not found: {:?}", input))?;
                    stored
                }
            };
            if utxo.owner != tx.sender {
                bail!(
                    "UTxO input {:?} owned by {:?}, not sender {:?}",
                    input,
                    utxo.owner,
                    tx.sender
                );
            }
            total_input = total_input
                .checked_add(utxo.amount)
                .ok_or_else(|| anyhow!("UTxO total input overflow"))?;
        }

        // Validate UTxO outputs and balance
        let mut total_output = 0u128;
        for output in &tx.outputs {
            total_output = total_output
                .checked_add(output.amount)
                .ok_or_else(|| anyhow!("UTxO total output overflow"))?;
        }
        // Validate UTxO balance: inputs must cover outputs + fee
        if is_utxo_tx {
            let required = total_output
                .checked_add(tx.fee)
                .ok_or_else(|| anyhow!("UTxO output + fee overflow"))?;
            if total_input < required {
                bail!("UTxO inputs insufficient for outputs + fee");
            }
        } else if total_input < total_output {
            bail!("UTxO inputs insufficient for outputs");
        }

        // Contract deploy/call runs last: it is the most expensive check and
        // its effects are only applied if everything else validated.
        let contract_effects = if transfer_payload.is_none() && !is_utxo_tx {
            self.execute_contract(tx, overlay, context)?
        } else {
            None
        };

        // --- All validation passed; now write to overlay ---

        // Write account changes to overlay
        let sender_bytes = bincode::serialize(&sender_account)?;
        overlay.put(CF_ACCOUNTS, tx.sender.as_bytes().to_vec(), sender_bytes);
        overlay.changed_accounts.push(tx.sender);

        // Update speculative merkle tree (root recomputed once per block)
        let sender_hash = self.hash_account(&sender_account);
        spec_tree.set_leaf(sender_account.address, sender_hash);

        if let Some(ref recipient) = recipient_account {
            let recipient_bytes = bincode::serialize(recipient)?;
            overlay.put(
                CF_ACCOUNTS,
                recipient.address.as_bytes().to_vec(),
                recipient_bytes,
            );
            overlay.changed_accounts.push(recipient.address);

            let recipient_hash = self.hash_account(recipient);
            spec_tree.set_leaf(recipient.address, recipient_hash);
        }

        let mut gas_used = 0u64;
        let mut logs = Vec::new();
        if let Some(effects) = contract_effects {
            let address = effects.account.address;
            if let Some((code_hash, code)) = effects.code {
                overlay.put(CF_METADATA, contract_code_key(&code_hash), code);
            }
            overlay.put(
                CF_METADATA,
                contract_storage_key(&address),
                effects.storage_bytes,
            );
            overlay.put(
                CF_ACCOUNTS,
                address.as_bytes().to_vec(),
                bincode::serialize(&effects.account)?,
            );
            overlay.changed_accounts.push(address);
            spec_tree.set_leaf(address, self.hash_account(&effects.account));
            gas_used = effects.gas_used;
            logs = effects
                .logs
                .into_iter()
                .map(|data| aether_types::transaction::Log {
                    address,
                    topics: vec![],
                    data,
                })
                .collect();
        }

        // Delete consumed UTxOs from overlay
        for input in &tx.inputs {
            let key = bincode::serialize(input)?;
            overlay.delete(CF_UTXOS, key);
        }
        let tx_hash = tx.hash();
        for (idx, output) in tx.outputs.iter().enumerate() {
            let output_index: u32 = u32::try_from(idx)
                .map_err(|_| anyhow!("transaction output index exceeds u32::MAX"))?;
            let utxo_id = UtxoId {
                tx_hash,
                output_index,
            };
            let utxo = Utxo {
                amount: output.amount,
                owner: output.owner.to_address(),
                script_hash: output.script_hash,
            };
            let key = bincode::serialize(&utxo_id)?;
            let value = bincode::serialize(&utxo)?;
            overlay.put(CF_UTXOS, key, value);
        }

        Ok(TransactionReceipt {
            tx_hash,
            block_hash: H256::zero(),
            slot: 0,
            status: TransactionStatus::Success,
            gas_used,
            logs,
            // Filled with the post-block root by `apply_block_on`.
            state_root: H256::zero(),
        })
    }

    /// Read an account from overlay first, then fall back to storage.
    fn get_account_from_overlay(
        &self,
        overlay: &PendingOverlay,
        address: &Address,
    ) -> Result<Account> {
        if let Some(maybe_bytes) = overlay.get(CF_ACCOUNTS, address.as_bytes()) {
            match maybe_bytes {
                Some(bytes) => Ok(bincode::deserialize(&bytes)?),
                None => Ok(Account::new(*address)), // Deleted in overlay → new account
            }
        } else {
            self.get_or_create_account(address)
        }
    }

    /// Build a StorageBatch from a speculative overlay WITHOUT writing to disk.
    /// Updates the in-memory merkle tree and includes the state root in the batch.
    /// The caller can extend this batch with additional data (e.g. block/receipt
    /// persistence) before writing, ensuring a single atomic commit.
    pub fn prepare_overlay_batch(&mut self, overlay: &PendingOverlay) -> Result<StorageBatch> {
        let mut batch = StorageBatch::new();
        for ((cf, key), value) in &overlay.writes {
            batch.put(cf, key.clone(), value.clone());
        }
        for (cf, key) in &overlay.deletes {
            batch.delete(cf, key.clone());
        }

        // Update merkle tree with changed accounts
        for addr in &overlay.changed_accounts {
            if let Some(maybe_bytes) = overlay.get(CF_ACCOUNTS, addr.as_bytes()) {
                match maybe_bytes {
                    Some(bytes) => {
                        let account: Account = bincode::deserialize(&bytes)?;
                        let hash = self.hash_account(&account);
                        self.merkle_tree.update(*addr, hash);
                    }
                    None => {
                        self.merkle_tree.update(*addr, H256::zero());
                    }
                }
            }
        }

        // Include state root in the batch
        let root = self.merkle_tree.root();
        batch.put(
            CF_METADATA,
            b"state_root".to_vec(),
            root.as_bytes().to_vec(),
        );

        Ok(batch)
    }

    /// Append spent-UTXO records to `batch` for all UTXOs deleted in `overlay`.
    ///
    /// Each spent UTXO is recorded in CF_SPENT_UTXOS with key = `slot (8-byte BE) + utxo_id`
    /// and value = `spender_address (20 bytes)`.  This enables light clients to verify
    /// that a UTXO was consumed at a specific slot, and epoch-based pruning to reclaim
    /// the space once the records are no longer needed.
    pub fn record_spent_utxos(
        &self,
        batch: &mut StorageBatch,
        overlay: &PendingOverlay,
        slot: u64,
    ) {
        let slot_prefix = slot.to_be_bytes();
        for (cf, utxo_key) in &overlay.deletes {
            if cf == CF_UTXOS {
                let mut key = Vec::with_capacity(8 + utxo_key.len());
                key.extend_from_slice(&slot_prefix);
                key.extend_from_slice(utxo_key);
                // Value: empty (the key itself identifies which UTXO was spent at which slot).
                // A full audit trail would store spender/amount, but for pruning purposes
                // the existence of the record is sufficient.
                batch.put(CF_SPENT_UTXOS, key, Vec::new());
            }
        }
    }

    /// Commit a speculative overlay to permanent storage.
    /// All state changes (accounts, UTXOs, state root) are written in a single
    /// atomic WriteBatch so a crash mid-commit cannot corrupt state.
    pub fn commit_overlay(&mut self, overlay: PendingOverlay) -> Result<()> {
        let _span = tracing::info_span!("commit_overlay").entered();
        let batch = self.prepare_overlay_batch(&overlay)?;
        self.storage.write_batch(batch)?;
        Ok(())
    }

    /// Write a pre-built StorageBatch to disk.
    /// Used when the caller has combined multiple logical operations (e.g. overlay
    /// commit + block persistence) into a single atomic batch.
    pub fn write_batch(&self, batch: StorageBatch) -> Result<()> {
        self.storage.write_batch(batch)?;
        Ok(())
    }

    pub fn seed_account(&mut self, address: &Address, balance: u128) -> Result<()> {
        let account = Account::with_balance(*address, balance);
        let mut batch = StorageBatch::new();
        let key = address.as_bytes().to_vec();
        let value = bincode::serialize(&account)?;
        batch.put(CF_ACCOUNTS, key, value);
        // Include state root in same atomic batch
        self.update_state_root_incremental(&account, None, Some(&mut batch))?;
        self.storage.write_batch(batch)?;
        Ok(())
    }

    /// Seed many genesis balances in one atomic batch (one Merkle recompute).
    pub fn seed_accounts(&mut self, accounts: &[(Address, u128)]) -> Result<()> {
        let mut batch = StorageBatch::new();
        for (address, balance) in accounts {
            let account = Account::with_balance(*address, *balance);
            batch.put(
                CF_ACCOUNTS,
                address.as_bytes().to_vec(),
                bincode::serialize(&account)?,
            );
            self.merkle_tree
                .set_leaf(*address, self.hash_account(&account));
        }
        self.merkle_tree.recompute();
        batch.put(
            CF_METADATA,
            b"state_root".to_vec(),
            self.merkle_tree.root().as_bytes().to_vec(),
        );
        self.storage.write_batch(batch)?;
        Ok(())
    }

    /// Insert a UTxO directly into storage for testing.
    /// Only available in test builds — production code creates UTxOs via transaction outputs.
    #[cfg(test)]
    pub fn seed_utxo(&mut self, utxo_id: &UtxoId, amount: u128, owner: Address) -> Result<()> {
        let utxo = aether_types::Utxo {
            amount,
            owner,
            script_hash: None,
        };
        let mut batch = StorageBatch::new();
        let key = bincode::serialize(utxo_id)?;
        let value = bincode::serialize(&utxo)?;
        batch.put(CF_UTXOS, key, value);
        self.storage.write_batch(batch)?;
        Ok(())
    }

    pub fn apply_block_transactions(
        &mut self,
        transactions: &[Transaction],
    ) -> Result<Vec<TransactionReceipt>> {
        let _span = tracing::info_span!("apply_block_transactions", tx_count = transactions.len(),)
            .entered();
        let mut receipts = Vec::new();

        if transactions.is_empty() {
            return Ok(receipts);
        }

        for tx in transactions {
            let derived = tx.sender_pubkey.to_address();
            if derived != tx.sender {
                bail!(
                    "sender address does not match public key: tx_hash={}",
                    tx.hash()
                );
            }
        }

        let batch_inputs: Vec<_> = transactions.iter().map(|tx| tx.ed25519_tuple()).collect();
        let batch_results = ed25519::verify_batch(&batch_inputs)
            .map_err(|e| anyhow!("batch signature verification failed: {e:?}"))?;

        for (tx, is_valid) in transactions.iter().zip(batch_results.iter()) {
            if !*is_valid {
                bail!(
                    "block contains transaction with invalid signature: tx_hash={}",
                    tx.hash()
                );
            }
        }

        for tx in transactions.iter() {
            match self.apply_transaction_validated(tx) {
                Ok(receipt) => receipts.push(receipt),
                Err(e) => {
                    // Transaction failed, still include receipt
                    receipts.push(TransactionReceipt {
                        tx_hash: tx.hash(),
                        block_hash: H256::zero(),
                        slot: 0,
                        status: TransactionStatus::Failed {
                            reason: e.to_string(),
                        },
                        gas_used: 0,
                        logs: vec![],
                        state_root: self.state_root(),
                    });
                }
            }
        }

        Ok(receipts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_crypto_primitives::Keypair;
    use aether_types::{PublicKey, Signature, TransferPayload, TRANSFER_PROGRAM_ID};
    use std::collections::HashSet;
    use tempfile::TempDir;

    #[test]
    fn test_account_creation() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();

        let account = ledger.get_or_create_account(&address).unwrap();
        assert_eq!(account.balance, 0);
        assert_eq!(account.nonce, 0);
    }

    #[test]
    fn test_simple_transfer() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();

        // Give account some balance
        let account = Account::with_balance(address, 1000);
        let mut batch = StorageBatch::new();
        let key = address.as_bytes().to_vec();
        let value = bincode::serialize(&account).unwrap();
        batch.put(CF_ACCOUNTS, key, value);
        ledger.storage.write_batch(batch).unwrap();

        // Create transaction
        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        let signature = keypair.sign(hash.as_bytes());
        tx.signature = Signature::from_bytes(signature);

        let receipt = ledger.apply_transaction(&tx).unwrap();
        assert!(matches!(receipt.status, TransactionStatus::Success));
    }

    #[test]
    fn test_batch_verification_marks_invalid_signatures() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();

        // Seed balance
        let account = Account::with_balance(address, 1_000);
        let mut batch = StorageBatch::new();
        let key = address.as_bytes().to_vec();
        let value = bincode::serialize(&account).unwrap();
        batch.put(CF_ACCOUNTS, key, value);
        ledger.storage.write_batch(batch).unwrap();

        // Build signed transaction
        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        let signature = keypair.sign(hash.as_bytes());
        tx.signature = Signature::from_bytes(signature.clone());

        let mut invalid_tx = tx.clone();
        invalid_tx.signature = Signature::from_bytes(vec![0; 64]);

        // Block containing an invalid-signature tx must be rejected entirely —
        // a valid block should never include garbage transactions.
        let err = ledger
            .apply_block_transactions(&[tx.clone(), invalid_tx])
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid signature"),
            "expected invalid signature error, got: {err}"
        );

        // A block with only valid transactions should succeed
        let receipts = ledger.apply_block_transactions(&[tx]).unwrap();
        assert_eq!(receipts.len(), 1);
        assert!(matches!(receipts[0].status, TransactionStatus::Success));
    }

    #[test]
    fn test_sender_pubkey_mismatch_rejected_in_batch_paths() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let victim_keypair = Keypair::generate();
        let victim_address = Address::from_slice(&victim_keypair.to_address()).unwrap();
        let attacker_keypair = Keypair::generate();

        let account = Account::with_balance(victim_address, 1_000_000);
        let mut batch = StorageBatch::new();
        batch.put(
            CF_ACCOUNTS,
            victim_address.as_bytes().to_vec(),
            bincode::serialize(&account).unwrap(),
        );
        ledger.storage.write_batch(batch).unwrap();

        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: victim_address,
            sender_pubkey: PublicKey::from_bytes(attacker_keypair.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(attacker_keypair.sign(hash.as_bytes()));

        let err = ledger.apply_block_speculatively(&[tx.clone()]).unwrap_err();
        assert!(
            err.to_string()
                .contains("sender address does not match public key"),
            "expected sender-pubkey mismatch error in speculative path, got: {err}"
        );

        let err = ledger.apply_block_transactions(&[tx.clone()]).unwrap_err();
        assert!(
            err.to_string()
                .contains("sender address does not match public key"),
            "expected sender-pubkey mismatch error in block_transactions path, got: {err}"
        );

        let err = ledger.apply_transaction(&tx).unwrap_err();
        assert!(
            err.to_string()
                .contains("sender address does not match public key"),
            "expected sender-pubkey mismatch error in individual path, got: {err}"
        );
    }

    #[test]
    fn test_transfer_program_moves_balance() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let sender_key = Keypair::generate();
        let sender = Address::from_slice(&sender_key.to_address()).unwrap();
        let recipient = Address::from_slice(&[9u8; 20]).unwrap();

        let mut seed_batch = StorageBatch::new();
        seed_batch.put(
            CF_ACCOUNTS,
            sender.as_bytes().to_vec(),
            bincode::serialize(&Account::with_balance(sender, 100_000)).unwrap(),
        );
        ledger.storage.write_batch(seed_batch).unwrap();

        let payload = TransferPayload {
            recipient,
            amount: 1_500,
            memo: Some("ledger test".to_string()),
        };
        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender,
            sender_pubkey: PublicKey::from_bytes(sender_key.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: Some(TRANSFER_PROGRAM_ID),
            data: bincode::serialize(&payload).unwrap(),
            gas_limit: 21_000,
            fee: 400,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(sender_key.sign(hash.as_bytes()));

        let receipt = ledger.apply_transaction(&tx).unwrap();
        assert!(matches!(receipt.status, TransactionStatus::Success));

        let sender_after = ledger.get_account(&sender).unwrap().unwrap();
        let recipient_after = ledger.get_account(&recipient).unwrap().unwrap();
        assert_eq!(sender_after.nonce, 1);
        assert_eq!(sender_after.balance, 98_100);
        assert_eq!(recipient_after.balance, 1_500);
    }

    #[test]
    fn test_state_root_persisted_atomically_with_accounts() {
        // Verify that after a transaction, the state root stored in metadata
        // matches the in-memory merkle tree — proving they were written together.
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();
        ledger.seed_account(&address, 10_000).unwrap();

        let root_after_seed = ledger.state_root();
        let stored_root = ledger
            .storage()
            .get(CF_METADATA, b"state_root")
            .unwrap()
            .map(|b| H256::from_slice(&b).unwrap());
        assert_eq!(Some(root_after_seed), stored_root);

        // Apply a transaction and verify consistency again
        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));
        ledger.apply_transaction(&tx).unwrap();

        let root_after_tx = ledger.state_root();
        let stored_root2 = ledger
            .storage()
            .get(CF_METADATA, b"state_root")
            .unwrap()
            .map(|b| H256::from_slice(&b).unwrap());
        assert_eq!(Some(root_after_tx), stored_root2);
        assert_ne!(root_after_seed, root_after_tx);
    }

    #[test]
    fn test_overlay_commit_atomic_state_root() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();
        ledger.seed_account(&address, 50_000).unwrap();

        // Build and commit an overlay
        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 200,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));

        let (receipts, overlay) = ledger.apply_block_speculatively(&[tx]).unwrap();
        assert!(matches!(receipts[0].status, TransactionStatus::Success));

        let overlay_root = overlay.state_root;
        ledger.commit_overlay(overlay).unwrap();

        // After commit, stored root must match overlay's computed root
        let stored_root = ledger
            .storage()
            .get(CF_METADATA, b"state_root")
            .unwrap()
            .map(|b| H256::from_slice(&b).unwrap());
        assert_eq!(Some(overlay_root), stored_root);
        assert_eq!(ledger.state_root(), overlay_root);
    }

    #[test]
    fn test_prepare_overlay_batch_and_extend() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();
        ledger.seed_account(&address, 50_000).unwrap();

        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 200,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));

        let (_receipts, overlay) = ledger.apply_block_speculatively(&[tx]).unwrap();
        let overlay_root = overlay.state_root;

        // Build overlay batch WITHOUT writing
        let mut batch = ledger.prepare_overlay_batch(&overlay).unwrap();

        // Nothing should be persisted yet — state_root in DB is still the seed root
        let root_before = ledger
            .storage()
            .get(CF_METADATA, b"state_root")
            .unwrap()
            .map(|b| H256::from_slice(&b).unwrap());
        assert_ne!(
            root_before,
            Some(overlay_root),
            "batch should not be written yet"
        );

        // Extend with extra data (simulating block persistence)
        let mut extra = StorageBatch::new();
        extra.put(CF_METADATA, b"test_key".to_vec(), b"test_value".to_vec());
        batch.extend(extra);

        // Single atomic write
        ledger.write_batch(batch).unwrap();

        // Now both overlay state AND extra data should be persisted
        let stored_root = ledger
            .storage()
            .get(CF_METADATA, b"state_root")
            .unwrap()
            .map(|b| H256::from_slice(&b).unwrap());
        assert_eq!(Some(overlay_root), stored_root);

        let extra_val = ledger.storage().get(CF_METADATA, b"test_key").unwrap();
        assert_eq!(extra_val, Some(b"test_value".to_vec()));
    }

    #[test]
    fn test_nonce_replay_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();

        // Seed balance
        let account = Account::with_balance(address, 10_000);
        let mut batch = StorageBatch::new();
        batch.put(
            CF_ACCOUNTS,
            address.as_bytes().to_vec(),
            bincode::serialize(&account).unwrap(),
        );
        ledger.storage.write_batch(batch).unwrap();

        // First tx with nonce 0 succeeds
        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));

        let receipt = ledger.apply_transaction(&tx).unwrap();
        assert!(matches!(receipt.status, TransactionStatus::Success));

        // Replay same tx (nonce 0 again) must fail
        let err = ledger.apply_transaction(&tx).unwrap_err();
        assert!(
            err.to_string().contains("invalid nonce"),
            "replay should be rejected with nonce error, got: {}",
            err
        );
    }

    #[test]
    fn test_nonce_must_be_sequential() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();

        let account = Account::with_balance(address, 10_000);
        let mut batch = StorageBatch::new();
        batch.put(
            CF_ACCOUNTS,
            address.as_bytes().to_vec(),
            bincode::serialize(&account).unwrap(),
        );
        ledger.storage.write_batch(batch).unwrap();

        // Skip nonce 0, try nonce 1 — must fail
        let mut tx = Transaction {
            nonce: 1,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));

        let err = ledger.apply_transaction(&tx).unwrap_err();
        assert!(
            err.to_string().contains("invalid nonce"),
            "out-of-order nonce should be rejected, got: {}",
            err
        );
    }

    #[test]
    fn test_cross_chain_replay_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();

        let account = Account::with_balance(address, 10_000);
        let mut batch = StorageBatch::new();
        batch.put(
            CF_ACCOUNTS,
            address.as_bytes().to_vec(),
            bincode::serialize(&account).unwrap(),
        );
        ledger.storage.write_batch(batch).unwrap();

        // Tx with chain_id=999 (wrong chain)
        let mut tx = Transaction {
            nonce: 0,
            chain_id: 999,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));

        // With chain_id enforcement, tx should be rejected
        let (receipts, _overlay) = ledger
            .apply_block_speculatively_with_chain_id(&[tx], Some(900))
            .unwrap();
        assert_eq!(receipts.len(), 1);
        assert!(
            matches!(&receipts[0].status, TransactionStatus::Failed { reason } if reason.contains("wrong chain_id")),
            "cross-chain tx should be rejected, got: {:?}",
            receipts[0].status
        );
    }

    #[test]
    fn test_intra_block_nonce_replay_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();

        let account = Account::with_balance(address, 10_000);
        let mut batch = StorageBatch::new();
        batch.put(
            CF_ACCOUNTS,
            address.as_bytes().to_vec(),
            bincode::serialize(&account).unwrap(),
        );
        ledger.storage.write_batch(batch).unwrap();

        // Two identical transactions in the same block (same nonce)
        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));

        let (receipts, _overlay) = ledger.apply_block_speculatively(&[tx.clone(), tx]).unwrap();
        assert_eq!(receipts.len(), 2);
        assert!(matches!(receipts[0].status, TransactionStatus::Success));
        assert!(
            matches!(&receipts[1].status, TransactionStatus::Failed { reason } if reason.contains("invalid nonce")),
            "duplicate nonce in same block should be rejected, got: {:?}",
            receipts[1].status
        );
    }

    #[test]
    fn test_utxo_fee_not_double_charged() {
        // Regression test: UTxO transactions should pay fees from the UTxO surplus
        // (total_input - total_output), NOT from the sender's account balance.
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();

        // Give account balance=500 and a UTxO worth 1000
        ledger.seed_account(&address, 500).unwrap();

        // Create a UTxO in storage
        let fake_tx_hash = H256::from_slice(&[0xAA; 32]).unwrap();
        let utxo_id = UtxoId {
            tx_hash: fake_tx_hash,
            output_index: 0,
        };
        let utxo = Utxo {
            amount: 1000,
            owner: address,
            script_hash: None,
        };
        let mut batch = StorageBatch::new();
        let key = bincode::serialize(&utxo_id).unwrap();
        let value = bincode::serialize(&utxo).unwrap();
        batch.put(CF_UTXOS, key, value);
        ledger.storage.write_batch(batch).unwrap();

        // Spend the UTxO: 1000 in, 900 out, fee=100. Surplus covers fee exactly.
        let recipient_kp = Keypair::generate();
        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![utxo_id],
            outputs: vec![aether_types::UtxoOutput {
                amount: 900,
                owner: PublicKey::from_bytes(recipient_kp.public_key()),
                script_hash: None,
            }],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));

        let receipt = ledger.apply_transaction(&tx).unwrap();
        assert!(
            matches!(receipt.status, TransactionStatus::Success),
            "UTxO tx should succeed; got: {:?}",
            receipt.status
        );

        // Account balance should be UNCHANGED (fee came from UTxO surplus, not account)
        let sender = ledger.get_account(&address).unwrap().unwrap();
        assert_eq!(
            sender.balance, 500,
            "account balance must not be debited for UTxO tx fee"
        );
        assert_eq!(sender.nonce, 1);
    }

    #[test]
    fn test_utxo_insufficient_surplus_for_fee() {
        // UTxO tx where input - output < fee should be rejected
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();
        ledger.seed_account(&address, 10_000).unwrap();

        let fake_tx_hash = H256::from_slice(&[0xBB; 32]).unwrap();
        let utxo_id = UtxoId {
            tx_hash: fake_tx_hash,
            output_index: 0,
        };
        let utxo = Utxo {
            amount: 1000,
            owner: address,
            script_hash: None,
        };
        let mut batch = StorageBatch::new();
        let key = bincode::serialize(&utxo_id).unwrap();
        let value = bincode::serialize(&utxo).unwrap();
        batch.put(CF_UTXOS, key, value);
        ledger.storage.write_batch(batch).unwrap();

        // 1000 in, 950 out, fee=100 => surplus=50 < fee=100 => should fail
        let recipient_kp = Keypair::generate();
        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![utxo_id],
            outputs: vec![aether_types::UtxoOutput {
                amount: 950,
                owner: PublicKey::from_bytes(recipient_kp.public_key()),
                script_hash: None,
            }],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));

        let result = ledger.apply_transaction(&tx);
        assert!(
            result.is_err(),
            "UTxO tx with insufficient surplus for fee should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("insufficient for outputs + fee"),
            "expected fee-related error, got: {}",
            err
        );
    }

    #[test]
    fn test_utxo_fee_in_speculative_path() {
        // Same double-charge fix verified through the speculative (overlay) path
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();
        ledger.seed_account(&address, 500).unwrap();

        let fake_tx_hash = H256::from_slice(&[0xCC; 32]).unwrap();
        let utxo_id = UtxoId {
            tx_hash: fake_tx_hash,
            output_index: 0,
        };
        let utxo = Utxo {
            amount: 1000,
            owner: address,
            script_hash: None,
        };
        let mut batch = StorageBatch::new();
        let key = bincode::serialize(&utxo_id).unwrap();
        let value = bincode::serialize(&utxo).unwrap();
        batch.put(CF_UTXOS, key, value);
        ledger.storage.write_batch(batch).unwrap();

        let recipient_kp = Keypair::generate();
        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![utxo_id],
            outputs: vec![aether_types::UtxoOutput {
                amount: 900,
                owner: PublicKey::from_bytes(recipient_kp.public_key()),
                script_hash: None,
            }],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));

        let (receipts, _overlay) = ledger.apply_block_speculatively(&[tx]).unwrap();
        assert_eq!(receipts.len(), 1);
        assert!(
            matches!(receipts[0].status, TransactionStatus::Success),
            "speculative UTxO tx should succeed; got: {:?}",
            receipts[0].status
        );
    }

    #[test]
    fn test_overlay_rejects_transfer_with_utxo_mixing() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();
        ledger.seed_account(&address, 1_000_000).unwrap();

        let recipient_keypair = Keypair::generate();
        let recipient = Address::from_slice(&recipient_keypair.to_address()).unwrap();

        let payload = TransferPayload {
            recipient,
            amount: 1_000,
            memo: None,
        };
        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![],
            outputs: vec![aether_types::UtxoOutput {
                amount: 999_999,
                owner: PublicKey::from_bytes(keypair.public_key()),
                script_hash: None,
            }],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: Some(TRANSFER_PROGRAM_ID),
            data: bincode::serialize(&payload).unwrap(),
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));

        let (receipts, _overlay) = ledger.apply_block_speculatively(&[tx]).unwrap();
        assert_eq!(receipts.len(), 1);
        assert!(
            matches!(&receipts[0].status, TransactionStatus::Failed { reason } if reason.contains("cannot mix")),
            "transfer+UTxO mixing should be rejected in overlay path, got: {:?}",
            receipts[0].status
        );
    }

    /// Regression test: a transaction that lists the same UTxO input twice
    /// should be rejected, not count the input value double.
    #[test]
    fn test_duplicate_utxo_input_rejected_apply_transaction() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();

        // Seed account for nonce tracking
        let account = Account::with_balance(address, 0);
        let mut batch = StorageBatch::new();
        batch.put(
            CF_ACCOUNTS,
            address.as_bytes().to_vec(),
            bincode::serialize(&account).unwrap(),
        );

        // Create a UTxO worth 500
        let utxo_id = UtxoId {
            tx_hash: H256::zero(),
            output_index: 0,
        };
        let utxo = Utxo {
            amount: 500,
            owner: address,
            script_hash: None,
        };
        batch.put(
            CF_UTXOS,
            bincode::serialize(&utxo_id).unwrap(),
            bincode::serialize(&utxo).unwrap(),
        );
        ledger.storage.write_batch(batch).unwrap();

        // Build a tx that lists the same UTxO input TWICE, trying to claim 1000
        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![utxo_id.clone(), utxo_id.clone()],
            outputs: vec![aether_types::UtxoOutput {
                amount: 900,
                owner: PublicKey::from_bytes(keypair.public_key()),
                script_hash: None,
            }],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));

        let result = ledger.apply_transaction(&tx);
        assert!(result.is_err(), "duplicate UTxO input must be rejected");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("duplicate UTxO input"),
            "error should mention duplicate input"
        );
    }

    /// Same duplicate-input check must work in the speculative (overlay) path.
    #[test]
    fn test_duplicate_utxo_input_rejected_speculative() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();

        // Seed account
        let account = Account::with_balance(address, 0);
        let mut batch = StorageBatch::new();
        batch.put(
            CF_ACCOUNTS,
            address.as_bytes().to_vec(),
            bincode::serialize(&account).unwrap(),
        );

        // Create a UTxO worth 500
        let utxo_id = UtxoId {
            tx_hash: H256::zero(),
            output_index: 0,
        };
        let utxo = Utxo {
            amount: 500,
            owner: address,
            script_hash: None,
        };
        batch.put(
            CF_UTXOS,
            bincode::serialize(&utxo_id).unwrap(),
            bincode::serialize(&utxo).unwrap(),
        );
        ledger.storage.write_batch(batch).unwrap();

        // Build tx with duplicate input
        let mut tx = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![utxo_id.clone(), utxo_id.clone()],
            outputs: vec![aether_types::UtxoOutput {
                amount: 900,
                owner: PublicKey::from_bytes(keypair.public_key()),
                script_hash: None,
            }],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash = tx.hash();
        tx.signature = Signature::from_bytes(keypair.sign(hash.as_bytes()));

        let (receipts, _overlay) = ledger.apply_block_speculatively(&[tx]).unwrap();
        assert_eq!(receipts.len(), 1);
        assert!(
            matches!(&receipts[0].status, TransactionStatus::Failed { reason } if reason.contains("duplicate UTxO input")),
            "speculative path must reject duplicate UTxO inputs, got: {:?}",
            receipts[0].status
        );
    }

    /// Cross-tx double-spend within a single block: tx1 spends a UTxO,
    /// tx2 tries to spend the same UTxO — tx2 must fail.
    #[test]
    fn test_cross_tx_double_spend_in_block_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();

        // Seed account
        let account = Account::with_balance(address, 0);
        let mut batch = StorageBatch::new();
        batch.put(
            CF_ACCOUNTS,
            address.as_bytes().to_vec(),
            bincode::serialize(&account).unwrap(),
        );

        // Create a single UTxO worth 1000
        let utxo_id = UtxoId {
            tx_hash: H256::zero(),
            output_index: 0,
        };
        let utxo = Utxo {
            amount: 1000,
            owner: address,
            script_hash: None,
        };
        batch.put(
            CF_UTXOS,
            bincode::serialize(&utxo_id).unwrap(),
            bincode::serialize(&utxo).unwrap(),
        );
        ledger.storage.write_batch(batch).unwrap();

        // tx1: spends the UTxO legitimately
        let mut tx1 = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![utxo_id.clone()],
            outputs: vec![aether_types::UtxoOutput {
                amount: 800,
                owner: PublicKey::from_bytes(keypair.public_key()),
                script_hash: None,
            }],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash1 = tx1.hash();
        tx1.signature = Signature::from_bytes(keypair.sign(hash1.as_bytes()));

        // tx2: tries to spend the same UTxO again
        let mut tx2 = Transaction {
            nonce: 1,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![utxo_id.clone()],
            outputs: vec![aether_types::UtxoOutput {
                amount: 800,
                owner: PublicKey::from_bytes(keypair.public_key()),
                script_hash: None,
            }],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash2 = tx2.hash();
        tx2.signature = Signature::from_bytes(keypair.sign(hash2.as_bytes()));

        let (receipts, _overlay) = ledger.apply_block_speculatively(&[tx1, tx2]).unwrap();
        assert_eq!(receipts.len(), 2);
        assert!(
            matches!(&receipts[0].status, TransactionStatus::Success),
            "tx1 should succeed"
        );
        assert!(
            matches!(&receipts[1].status, TransactionStatus::Failed { reason } if reason.contains("already spent")),
            "tx2 must fail as double-spend, got: {:?}",
            receipts[1].status
        );
    }

    #[test]
    fn test_fold_fee_distribution_credits_proposer() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let proposer_kp = Keypair::generate();
        let proposer = Address::from_slice(&proposer_kp.to_address()).unwrap();

        // Proposer starts with 0 balance.
        let overlay = PendingOverlay::new();
        let mut batch = StorageBatch::new();
        let reward = 5_000u128;
        ledger
            .fold_fee_distribution_into_batch(&mut batch, &overlay, &proposer, reward, 0, 0)
            .unwrap();
        ledger.write_batch(batch).unwrap();

        let account = ledger.get_or_create_account(&proposer).unwrap();
        assert_eq!(account.balance, reward, "proposer should receive reward");
    }

    #[test]
    fn test_fold_fee_distribution_records_burned() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let proposer_kp = Keypair::generate();
        let proposer = Address::from_slice(&proposer_kp.to_address()).unwrap();

        let overlay = PendingOverlay::new();
        let mut batch = StorageBatch::new();
        ledger
            .fold_fee_distribution_into_batch(&mut batch, &overlay, &proposer, 0, 1_000, 0)
            .unwrap();
        ledger.write_batch(batch).unwrap();

        assert_eq!(ledger.total_burned(), 1_000);
    }

    #[test]
    fn test_fold_fee_distribution_records_treasury_fees() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let proposer_kp = Keypair::generate();
        let proposer = Address::from_slice(&proposer_kp.to_address()).unwrap();

        let overlay = PendingOverlay::new();
        let mut batch = StorageBatch::new();
        ledger
            .fold_fee_distribution_into_batch(&mut batch, &overlay, &proposer, 0, 0, 500)
            .unwrap();
        ledger.write_batch(batch).unwrap();

        assert_eq!(ledger.total_treasury_fees(), 500);

        // Accumulates across blocks
        let mut batch2 = StorageBatch::new();
        ledger
            .fold_fee_distribution_into_batch(&mut batch2, &overlay, &proposer, 0, 0, 300)
            .unwrap();
        ledger.write_batch(batch2).unwrap();

        assert_eq!(ledger.total_treasury_fees(), 800);
    }

    #[test]
    fn test_fold_fee_distribution_state_root_includes_proposer_credit() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let proposer_kp = Keypair::generate();
        let proposer = Address::from_slice(&proposer_kp.to_address()).unwrap();

        let root_before = ledger.state_root();

        let overlay = PendingOverlay::new();
        let mut batch = StorageBatch::new();
        ledger
            .fold_fee_distribution_into_batch(&mut batch, &overlay, &proposer, 9_000, 0, 0)
            .unwrap();
        ledger.write_batch(batch).unwrap();

        let root_after = ledger.state_root();
        assert_ne!(
            root_before, root_after,
            "state root must change after credit"
        );
    }

    #[test]
    fn test_fold_fee_distribution_uses_overlay_for_proposer_balance() {
        // If the proposer has an updated balance in the overlay (e.g. they sent
        // a transaction in this block), fold_fee_distribution_into_batch must use
        // that overlay balance as the base for the credit, not the DB balance.
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let proposer_kp = Keypair::generate();
        let proposer = Address::from_slice(&proposer_kp.to_address()).unwrap();

        // Seed proposer with 1000 in the DB.
        ledger.seed_account(&proposer, 1_000).unwrap();

        // Build an overlay that reduces proposer balance to 800 (simulates a tx fee).
        let reduced_account = Account::with_balance(proposer, 800);
        let serialized = bincode::serialize(&reduced_account).unwrap();
        let mut overlay = PendingOverlay::new();
        overlay.put(CF_ACCOUNTS, proposer.as_bytes().to_vec(), serialized);
        overlay.changed_accounts.push(proposer);

        let mut batch = ledger.prepare_overlay_batch(&overlay).unwrap();
        ledger
            .fold_fee_distribution_into_batch(&mut batch, &overlay, &proposer, 200, 0, 0)
            .unwrap();
        ledger.write_batch(batch).unwrap();

        // Expected: overlay balance (800) + reward (200) = 1000
        let account = ledger.get_or_create_account(&proposer).unwrap();
        assert_eq!(
            account.balance, 1_000,
            "proposer balance should be overlay base (800) + reward (200)"
        );
    }

    #[test]
    fn test_record_spent_utxos_adds_entries() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage = aether_state_storage::Storage::open(temp_dir.path()).unwrap();
        let ledger = Ledger::new(storage).unwrap();

        // Build an overlay that deletes two UTXOs
        let mut overlay = PendingOverlay::new();
        let utxo_key1 = vec![1u8; 16];
        let utxo_key2 = vec![2u8; 16];
        overlay.delete(CF_UTXOS, utxo_key1.clone());
        overlay.delete(CF_UTXOS, utxo_key2.clone());
        // Also add a non-UTXO delete to verify it's ignored
        overlay.delete(CF_ACCOUNTS, vec![3u8; 20]);

        let mut batch = StorageBatch::new();
        ledger.record_spent_utxos(&mut batch, &overlay, 42);
        ledger.storage().write_batch(batch).unwrap();

        // Verify two spent-UTXO records exist at slot 42
        let prefix = 42u64.to_be_bytes();
        let records: Vec<_> = ledger
            .storage()
            .prefix_iterator(CF_SPENT_UTXOS, &prefix)
            .unwrap()
            .collect();
        assert_eq!(records.len(), 2, "should have 2 spent-UTXO records");

        // Verify key format: slot prefix + utxo key
        for (key, _) in &records {
            let slot = u64::from_be_bytes(key[..8].try_into().unwrap());
            assert_eq!(slot, 42);
        }
    }

    /// Regression test: a failed UTxO transaction in a speculative block must NOT
    /// corrupt the overlay (e.g., by incrementing the sender's nonce). If it did,
    /// a subsequent valid transaction from the same sender would fail with
    /// "invalid nonce".
    #[test]
    fn test_failed_utxo_tx_does_not_corrupt_overlay_nonce() {
        let temp_dir = TempDir::new().unwrap();
        let storage = Storage::open(temp_dir.path()).unwrap();
        let mut ledger = Ledger::new(storage).unwrap();

        let keypair = Keypair::generate();
        let address = Address::from_slice(&keypair.to_address()).unwrap();

        // Seed account with balance for the second (valid) transaction
        ledger.seed_account(&address, 10_000).unwrap();

        // tx1: UTxO transaction referencing a non-existent input — should FAIL
        let bad_utxo_id = UtxoId {
            tx_hash: H256::from_slice(&[0xDE; 32]).unwrap(),
            output_index: 0,
        };
        let mut tx1 = Transaction {
            nonce: 0,
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![bad_utxo_id],
            outputs: vec![aether_types::UtxoOutput {
                amount: 100,
                owner: PublicKey::from_bytes(keypair.public_key()),
                script_hash: None,
            }],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: None,
            data: vec![],
            gas_limit: 21_000,
            fee: 10,
            signature: Signature::from_bytes(vec![]),
        };
        let hash1 = tx1.hash();
        tx1.signature = Signature::from_bytes(keypair.sign(hash1.as_bytes()));

        // tx2: valid account-model transfer — should SUCCEED with nonce 0
        let recipient_keypair = Keypair::generate();
        let recipient = Address::from_slice(&recipient_keypair.to_address()).unwrap();
        let payload = TransferPayload {
            recipient,
            amount: 500,
            memo: None,
        };
        let mut tx2 = Transaction {
            nonce: 0, // Same nonce — tx1 should NOT have consumed it
            chain_id: 1,
            sender: address,
            sender_pubkey: PublicKey::from_bytes(keypair.public_key()),
            inputs: vec![],
            outputs: vec![],
            reads: HashSet::new(),
            writes: HashSet::new(),
            program_id: Some(TRANSFER_PROGRAM_ID),
            data: bincode::serialize(&payload).unwrap(),
            gas_limit: 21_000,
            fee: 100,
            signature: Signature::from_bytes(vec![]),
        };
        let hash2 = tx2.hash();
        tx2.signature = Signature::from_bytes(keypair.sign(hash2.as_bytes()));

        let (receipts, _overlay) = ledger.apply_block_speculatively(&[tx1, tx2]).unwrap();
        assert_eq!(receipts.len(), 2);

        // tx1 must have failed (non-existent UTxO)
        assert!(
            matches!(&receipts[0].status, TransactionStatus::Failed { .. }),
            "tx1 should fail, got: {:?}",
            receipts[0].status
        );

        // tx2 must succeed — the failed tx1 should NOT have corrupted the nonce
        assert!(
            matches!(&receipts[1].status, TransactionStatus::Success),
            "tx2 should succeed because tx1's failure did not corrupt the overlay nonce, got: {:?}",
            receipts[1].status
        );
    }
}
