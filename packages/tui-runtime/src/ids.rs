//! Stable opaque identifiers used by the runtime.

/// Application-owned key for a replaceable pending message.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageKey(String);

impl MessageKey {
    /// Create a message key.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Return the key text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for MessageKey {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for MessageKey {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// Application-owned key for a one-shot timer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimerId(String);

impl TimerId {
    /// Create a timer identifier.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Return the identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for TimerId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for TimerId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// Application-owned key for a long-lived subscription.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubscriptionKey(String);

impl SubscriptionKey {
    /// Create a subscription key.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Return the key text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for SubscriptionKey {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SubscriptionKey {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

/// Application-owned key for command lifecycle policy.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandKey(String);

impl CommandKey {
    /// Create a command key.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Return the key text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for CommandKey {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for CommandKey {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}
