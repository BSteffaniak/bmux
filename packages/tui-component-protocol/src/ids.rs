//! Stable identifiers used by component protocol messages.

/// Identifier for one declarative component node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct ComponentId(pub String);

impl ComponentId {
    /// Create a component identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ComponentId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ComponentId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Identifier for an action emitted by an interactive component.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct ActionId(pub String);

impl ActionId {
    /// Create an action identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Return the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ActionId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ActionId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}
