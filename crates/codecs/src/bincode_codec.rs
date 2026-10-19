use serde::{de::DeserializeOwned, Serialize};

use crate::error::{CodecError, Result};

/// Serialize a value using bincode with workspace defaults.
pub fn encode_bincode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    bincode::serialize(value).map_err(CodecError::from)
}

/// Deserialize a value encoded with bincode.
pub fn decode_bincode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    bincode::deserialize(bytes).map_err(CodecError::from)
}
