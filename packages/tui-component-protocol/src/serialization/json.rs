//! JSON serialization helpers for component protocol models.

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Serialize a protocol model to a JSON value.
///
/// # Errors
///
/// Returns an error when `serde_json` cannot represent the supplied value.
pub fn to_json_value<T>(value: &T) -> serde_json::Result<serde_json::Value>
where
    T: Serialize,
{
    serde_json::to_value(value)
}

/// Deserialize a protocol model from a JSON value.
///
/// # Errors
///
/// Returns an error when the JSON value does not match the requested model.
pub fn from_json_value<T>(value: serde_json::Value) -> serde_json::Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_value(value)
}

/// Serialize a protocol model to a JSON string.
///
/// # Errors
///
/// Returns an error when `serde_json` cannot serialize the supplied value.
pub fn to_json_string<T>(value: &T) -> serde_json::Result<String>
where
    T: Serialize,
{
    serde_json::to_string(value)
}

/// Deserialize a protocol model from a JSON string.
///
/// # Errors
///
/// Returns an error when the JSON string does not match the requested model.
pub fn from_json_str<T>(value: &str) -> serde_json::Result<T>
where
    T: DeserializeOwned,
{
    serde_json::from_str(value)
}
