//! Serialization-neutral component value model.

use std::collections::BTreeMap;

/// Value carried by component state and events.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ComponentValue {
    /// No value.
    Null,
    /// Boolean value.
    Bool(bool),
    /// Signed integer value.
    I64(i64),
    /// Unsigned integer value.
    U64(u64),
    /// Floating-point value.
    F64(f64),
    /// UTF-8 string value.
    String(String),
    /// Ordered list value.
    List(Vec<Self>),
    /// String-keyed map value.
    Map(BTreeMap<String, Self>),
}

impl ComponentValue {
    /// Return true when the value is [`ComponentValue::Null`].
    #[must_use]
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Return this value as a string slice when it is a string.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            Self::Null
            | Self::Bool(_)
            | Self::I64(_)
            | Self::U64(_)
            | Self::F64(_)
            | Self::List(_)
            | Self::Map(_) => None,
        }
    }
}

impl From<bool> for ComponentValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i64> for ComponentValue {
    fn from(value: i64) -> Self {
        Self::I64(value)
    }
}

impl From<u64> for ComponentValue {
    fn from(value: u64) -> Self {
        Self::U64(value)
    }
}

impl From<f64> for ComponentValue {
    fn from(value: f64) -> Self {
        Self::F64(value)
    }
}

impl From<String> for ComponentValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for ComponentValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}
