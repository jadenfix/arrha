use anyhow::Result;
use bincode::{deserialize, serialize};

use crate::shred::Shred;

pub fn serialize_shred(shred: &Shred) -> Result<Vec<u8>> {
    Ok(serialize(shred)?)
}

pub fn deserialize_shred(bytes: &[u8]) -> Result<Shred> {
    Ok(deserialize(bytes)?)
}
