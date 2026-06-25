use super::de;
use super::ser;
use crate::error::Error;
use crate::mode::EncodingMode;
use serde::{Deserialize, Serialize};

/// Serialize a value to typed-stable bytes.
///
/// # Errors
///
/// Returns an error if the value fails to serialize.
pub fn to_typed_vec<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    ser::to_vec_with_mode(value, EncodingMode::TypedStable)
}

/// Deserialize a value from typed-stable bytes.
///
/// # Errors
///
/// Returns an error if bytes cannot be decoded as `T`.
pub fn from_typed_bytes<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<T, Error> {
    de::from_bytes_with_mode(bytes, EncodingMode::TypedStable)
}
