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

/// Serialize a value to positional bytes, appending them to `output`.
///
/// # Errors
///
/// Returns an error if the value fails to serialize; `output` then holds whatever was written
/// before the failure.
pub fn append_positional<T: Serialize + ?Sized>(
    value: &T,
    output: &mut Vec<u8>,
) -> Result<(), Error> {
    ser::append_with_mode(value, EncodingMode::Positional, output)
}

/// Deserialize a value from positional bytes.
///
/// # Errors
///
/// Returns an error if the bytes cannot be deserialized into the target type.
pub fn from_positional_bytes<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<T, Error> {
    de::from_bytes_with_mode(bytes, EncodingMode::Positional)
}

/// Decode an untrusted positional value with entry and byte admission before
/// serde visitors can reserve containers. Fixed schema recursion remains the
/// caller's responsibility; these limits bound declared variable-size data.
///
/// # Errors
/// Rejects excessive lengths, invalid data and trailing bytes.
pub fn from_positional_bytes_bounded<'de, T: Deserialize<'de>>(
    bytes: &'de [u8],
    limits: crate::DecodeLimits,
) -> Result<T, Error> {
    de::from_bytes_bounded(bytes, limits)
}

#[cfg(test)]
mod tests {
    /// Appending writes what a fresh encoding would, after what the buffer held, into its storage.
    #[test]
    fn appending_matches_a_fresh_encoding_after_existing_bytes() {
        let value = (7_u32, "seven", Some(vec![1_u16, 2]));
        let mut output = b"head".to_vec();
        output.reserve(64);
        let storage = output.as_ptr();
        super::append_positional(&value, &mut output).unwrap();
        assert_eq!(&output[..4], b"head");
        assert_eq!(&output[4..], super::to_positional_vec(&value).unwrap());
        assert_eq!(output.as_ptr(), storage, "the buffer's storage is reused");
        let decoded: (u32, String, Option<Vec<u16>>) =
            super::from_positional_bytes(&output[4..]).unwrap();
        assert_eq!(decoded, (7, "seven".to_owned(), Some(vec![1, 2])));
    }
}
