use anyhow::Result;
use std::io::{Read, Write};

/// Compress data using zstd at level 3 (good speed/ratio balance).
pub fn compress(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = zstd::Encoder::new(Vec::new(), 3)?;
    encoder.write_all(bytes)?;
    let compressed = encoder.finish()?;
    Ok(compressed)
}

/// Decompress zstd-compressed data.
pub fn decompress(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = zstd::Decoder::new(bytes)?;
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;
    Ok(decompressed)
}
