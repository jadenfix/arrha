use borsh::{BorshDeserialize, BorshSerialize};

use crate::error::{CodecError, Result};

/// Serialize a value using Borsh.
pub fn encode_borsh<T: BorshSerialize>(value: &T) -> Result<Vec<u8>> {
    borsh::to_vec(value).map_err(CodecError::from)
}

/// Deserialize a value encoded with Borsh.
pub fn decode_borsh<T: BorshDeserialize>(bytes: &[u8]) -> Result<T> {
    T::try_from_slice(bytes).map_err(CodecError::from)
}
