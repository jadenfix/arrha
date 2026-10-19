use anyhow::{bail, Result};
use reed_solomon_erasure::galois_8::ReedSolomon;

/// Production Reed-Solomon decoder with error correction
/// Reconstructs original data from any k shards (data or parity)
#[derive(Debug)]
pub struct ReedSolomonDecoder {
    data_shards: usize,
    parity_shards: usize,
    decoder: ReedSolomon,
}

impl ReedSolomonDecoder {
    pub fn new(data_shards: usize, parity_shards: usize) -> Result<Self> {
        if data_shards == 0 {
            bail!("data shards must be non-zero");
        }
        if parity_shards == 0 {
            bail!("parity shards must be non-zero");
        }

        let decoder = ReedSolomon::new(data_shards, parity_shards)
            .map_err(|e| anyhow::anyhow!("failed to create RS decoder: {}", e))?;

        Ok(ReedSolomonDecoder {
            data_shards,
            parity_shards,
            decoder,
        })
    }

    /// Decode shards back into original data
    ///
    /// Accepts `Option<Vec<u8>>` to indicate present (Some) or missing (None) shards
    /// Requires at least k (data_shards) shards to be present
    pub fn decode(&self, shards: &[Option<Vec<u8>>]) -> Result<Vec<u8>> {
        let expected = self.data_shards + self.parity_shards;
        if shards.len() != expected {
            bail!(
                "expected {} shards (data + parity), got {}",
                expected,
                shards.len()
            );
        }

        // Count present shards
        let present_count = shards.iter().filter(|s| s.is_some()).count();
        if present_count < self.data_shards {
            bail!(
                "insufficient shards for reconstruction: need {}, have {}",
                self.data_shards,
                present_count
            );
        }

        // Clone shards for reconstruction (library needs owned data)
        let mut working_shards: Vec<Option<Vec<u8>>> = shards.to_vec();

        // Reconstruct missing shards
        self.decoder
            .reconstruct(&mut working_shards)
            .map_err(|e| anyhow::anyhow!("reconstruction failed: {}", e))?;

        // Concatenate data shards
        let mut data = Vec::new();
        for (i, shard_opt) in working_shards.iter().take(self.data_shards).enumerate() {
            if let Some(shard) = shard_opt {
                data.extend_from_slice(shard);
            } else {
                bail!("reconstruction failed: data shard {} still missing", i);
            }
        }

        // Read the length prefix and truncate to the original size
        if data.len() < 8 {
            bail!("decoded data too short: missing length prefix");
        }
        let raw_len = u64::from_le_bytes(
            data[..8]
                .try_into()
                .map_err(|_| anyhow::anyhow!("length prefix conversion failed"))?,
        );
        let original_len = usize::try_from(raw_len)
            .map_err(|_| anyhow::anyhow!("length prefix {} exceeds addressable range", raw_len))?;
        let end = 8usize
            .checked_add(original_len)
            .ok_or_else(|| anyhow::anyhow!("length prefix overflow: 8 + {}", original_len))?;
        if end > data.len() {
            bail!(
                "length prefix {} exceeds decoded data size {}",
                original_len,
                data.len() - 8
            );
        }
        Ok(data[8..end].to_vec())
    }

    pub fn shard_config(&self) -> (usize, usize) {
        (self.data_shards, self.parity_shards)
    }
}
