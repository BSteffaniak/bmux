use serde::{Deserializer, Serializer, de::Visitor, ser::Serialize};
use std::fmt;

/// Serialize a byte vector as codec bytes.
///
/// # Errors
///
/// Returns any serializer error from the underlying format.
pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_bytes(bytes)
}

/// Deserialize a byte vector through the format's byte-buffer path.
///
/// # Errors
///
/// Returns any deserializer error from the underlying format.
pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_byte_buf(ByteVecVisitor)
}

struct ByteVecVisitor;

impl<'de> Visitor<'de> for ByteVecVisitor {
    type Value = Vec<u8>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a byte buffer")
    }

    fn visit_borrowed_bytes<E>(self, value: &'de [u8]) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(value.to_vec())
    }

    fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(value.to_vec())
    }

    fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(value)
    }
}

struct ByteSlice<'a>(&'a [u8]);

impl Serialize for ByteSlice<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(self.0)
    }
}

/// Serde adapter for `Option<Vec<u8>>` fields that are semantically raw bytes.
pub mod option {
    use super::{ByteSlice, ByteVecVisitor};
    use serde::{Deserializer, Serializer, de::Visitor};
    use std::fmt;

    /// Serialize an optional byte vector as codec bytes when present.
    ///
    /// # Errors
    ///
    /// Returns any serializer error from the underlying format.
    pub fn serialize<S>(bytes: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match bytes {
            Some(bytes) => serializer.serialize_some(&ByteSlice(bytes)),
            None => serializer.serialize_none(),
        }
    }

    /// Deserialize optional codec bytes into a byte vector when present.
    ///
    /// # Errors
    ///
    /// Returns any deserializer error from the underlying format.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_option(OptionByteVecVisitor)
    }

    struct OptionByteVecVisitor;

    impl<'de> Visitor<'de> for OptionByteVecVisitor {
        type Value = Option<Vec<u8>>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an optional byte buffer")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_byte_buf(ByteVecVisitor).map(Some)
        }
    }
}
