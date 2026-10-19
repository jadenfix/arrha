use blake3::Hasher as Blake3Hasher;
use sha2::{Digest, Sha256};

#[inline]
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

#[inline]
#[must_use]
pub fn blake3_hash(data: &[u8]) -> [u8; 32] {
    let mut hasher = Blake3Hasher::new();
    hasher.update(data);
    hasher.finalize().into()
}

#[must_use]
pub fn hash_multiple(chunks: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for chunk in chunks {
        hasher.update(chunk);
    }
    hasher.finalize().into()
}
