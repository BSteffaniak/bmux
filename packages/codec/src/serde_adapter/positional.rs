use super::de;
use super::ser;
use crate::error::Error;
use crate::mode::EncodingMode;
use serde::{Deserialize, Serialize};

/// Serialize a value to positional bytes.
///
/// # Errors
///
/// Returns an error if the value fails to serialize.
pub fn to_positional_vec<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    ser::to_vec_with_mode(value, EncodingMode::Positional)
}

/// Deserialize a value from positional bytes.
///
/// # Errors
///
/// Returns an error if the bytes cannot be deserialized into the target type.
pub fn from_positional_bytes<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<T, Error> {
    de::from_bytes_with_mode(bytes, EncodingMode::Positional)
}
