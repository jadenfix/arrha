use aether_state_merkle::SparseMerkleTree;
use aether_state_storage::{Storage, StorageBatch, CF_ACCOUNTS, CF_METADATA, CF_UTXOS};
use anyhow::{bail, Result};
use sha2::{Digest, Sha256};

use crate::generator::{decode_snapshot, StateSnapshot};

pub fn import_snapshot(storage: &Storage, bytes: &[u8]) -> Result<StateSnapshot> {
    let snapshot = decode_snapshot(bytes)?;

    // Verify the snapshot's state root is non-zero to catch corruption.
    if snapshot.state_root == aether_types::H256::zero() {
        bail!("snapshot has zero state root — likely corrupted");
    }

    // Verify Merkle root: recompute from imported accounts and compare
    let mut verify_tree = SparseMerkleTree::new();
    for (address, account) in &snapshot.accounts {
        let account_bytes = bincode::serialize(account)?;
        let account_hash = Sha256::digest(&account_bytes);
        verify_tree.update(
            *address,
            aether_types::H256::from_slice(&account_hash).unwrap(),
        );
    }
    let computed_root = verify_tree.root();
    if computed_root != snapshot.state_root {
        bail!(
            "snapshot Merkle root mismatch: claimed {:?}, computed {:?} — data may be tampered",
            snapshot.state_root,
            computed_root
        );
    }

    let mut batch = StorageBatch::new();
    for (address, account) in &snapshot.accounts {
        let key = address.as_bytes().to_vec();
        let value = bincode::serialize(account)?;
        batch.put(CF_ACCOUNTS, key, value);
    }

    for (id, utxo) in &snapshot.utxos {
        let key = bincode::serialize(id)?;
        let value = bincode::serialize(utxo)?;
        batch.put(CF_UTXOS, key, value);
    }

    // Include metadata in the same batch for atomicity — a crash between
    // account writes and metadata writes would leave the DB in an inconsistent state.
    batch.put(
        CF_METADATA,
        b"state_root".to_vec(),
        snapshot.state_root.as_bytes().to_vec(),
    );
    batch.put(
        CF_METADATA,
        b"snapshot_height".to_vec(),
        snapshot.metadata.height.to_be_bytes().to_vec(),
    );
    storage.write_batch(batch)?;

    Ok(snapshot)
}
