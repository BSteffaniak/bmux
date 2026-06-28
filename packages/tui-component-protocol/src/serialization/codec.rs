//! BMUX codec serialization helpers for component protocol models.

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Serialize a protocol model to BMUX typed-stable bytes.
///
/// # Errors
///
/// Returns an error when `bmux_codec` cannot encode the supplied value.
pub fn to_typed_bytes<T>(value: &T) -> Result<Vec<u8>, bmux_codec::Error>
where
    T: Serialize,
{
    bmux_codec::to_typed_vec(value)
}

/// Deserialize a protocol model from BMUX typed-stable bytes.
///
/// # Errors
///
/// Returns an error when bytes cannot be decoded as the requested model.
pub fn from_typed_bytes<T>(bytes: &[u8]) -> Result<T, bmux_codec::Error>
where
    T: DeserializeOwned,
{
    bmux_codec::from_typed_bytes(bytes)
}

/// Serialize a protocol model to BMUX stable bytes.
///
/// # Errors
///
/// Returns an error when `bmux_codec` cannot encode the supplied value.
pub fn to_stable_bytes<T>(value: &T) -> Result<Vec<u8>, bmux_codec::Error>
where
    T: Serialize,
{
    bmux_codec::to_vec(value)
}

/// Deserialize a protocol model from BMUX stable bytes.
///
/// # Errors
///
/// Returns an error when bytes cannot be decoded as the requested model.
pub fn from_stable_bytes<T>(bytes: &[u8]) -> Result<T, bmux_codec::Error>
where
    T: DeserializeOwned,
{
    bmux_codec::from_bytes(bytes)
}
