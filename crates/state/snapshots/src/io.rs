use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::generator::{generate_snapshot, StateSnapshot};
use crate::importer::import_snapshot;
use aether_state_storage::Storage;

/// Name format: `snapshot_{height:010}.bin.zst`
fn snapshot_filename(height: u64) -> String {
    format!("snapshot_{height:010}.bin.zst")
}

/// Export a snapshot at the given height to disk.
///
/// The file is written atomically (write to temp, then rename) to prevent
/// partial files from being visible if the process crashes mid-write.
pub fn export_snapshot_to_file(
    storage: &Storage,
    height: u64,
    snapshot_dir: &Path,
) -> Result<PathBuf> {
    fs::create_dir_all(snapshot_dir)
        .with_context(|| format!("failed to create snapshot dir: {}", snapshot_dir.display()))?;

    let compressed = generate_snapshot(storage, height)?;

    let dest = snapshot_dir.join(snapshot_filename(height));
    let tmp = snapshot_dir.join(format!(".tmp_snapshot_{height}.bin.zst"));

    fs::write(&tmp, &compressed)
        .with_context(|| format!("failed to write temp snapshot: {}", tmp.display()))?;
    fs::rename(&tmp, &dest).with_context(|| {
        format!(
            "failed to rename snapshot: {} -> {}",
            tmp.display(),
            dest.display()
        )
    })?;

    Ok(dest)
}

/// Import a snapshot from a file on disk into storage.
pub fn import_snapshot_from_file(storage: &Storage, snapshot_path: &Path) -> Result<StateSnapshot> {
    if !snapshot_path.exists() {
        bail!("snapshot file not found: {}", snapshot_path.display());
    }
    let bytes = fs::read(snapshot_path)
        .with_context(|| format!("failed to read snapshot: {}", snapshot_path.display()))?;
    import_snapshot(storage, &bytes)
}

/// Retain only the most recent `keep` snapshots in the directory, deleting older ones.
///
/// Snapshots are identified by the `snapshot_*.bin.zst` naming convention and
/// sorted lexicographically (which equals chronological order due to zero-padded heights).
pub fn prune_old_snapshots(snapshot_dir: &Path, keep: usize) -> Result<usize> {
    if !snapshot_dir.exists() {
        return Ok(0);
    }

    let mut snapshot_files: Vec<PathBuf> = fs::read_dir(snapshot_dir)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("snapshot_")
                && name.ends_with(".bin.zst")
                && !name.starts_with(".tmp_")
            {
                Some(entry.path())
            } else {
                None
            }
        })
        .collect();

    snapshot_files.sort();

    let mut deleted = 0;
    if snapshot_files.len() > keep {
        let to_remove = snapshot_files.len() - keep;
        for path in &snapshot_files[..to_remove] {
            fs::remove_file(path)
                .with_context(|| format!("failed to delete old snapshot: {}", path.display()))?;
            deleted += 1;
        }
    }

    Ok(deleted)
}

/// List available snapshot heights in the directory, sorted ascending.
pub fn list_snapshots(snapshot_dir: &Path) -> Result<Vec<u64>> {
    if !snapshot_dir.exists() {
        return Ok(vec![]);
    }

    let mut heights: Vec<u64> = fs::read_dir(snapshot_dir)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().to_string();
            // Parse height from "snapshot_0000000100.bin.zst"
            let stem = name.strip_prefix("snapshot_")?.strip_suffix(".bin.zst")?;
            stem.parse::<u64>().ok()
        })
        .collect();

    heights.sort();
    Ok(heights)
}
