use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

const INVOCATION_ID_ALPHABET: [char; 64] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i',
    'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z', 'A', 'B',
    'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U',
    'V', 'W', 'X', 'Y', 'Z', '_', '-',
];
const INVOCATION_ID_LEN: usize = 21;

/// Serializable identifier that correlates all frames for one plugin invocation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginInvocationId(String);

impl PluginInvocationId {
    /// Create a new nanoid-backed invocation identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(nanoid::nanoid!(INVOCATION_ID_LEN, &INVOCATION_ID_ALPHABET))
    }

    /// Return the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for PluginInvocationId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for PluginInvocationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Cloneable host-owned cancellation state for a plugin invocation.
#[derive(Debug, Clone, Default)]
pub struct ServiceCancellation {
    cancelled: Arc<AtomicBool>,
}

impl ServiceCancellation {
    /// Create active, non-cancelled shared cancellation state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create active, non-cancelled shared cancellation state.
    #[must_use]
    pub fn none() -> Self {
        Self::new()
    }

    /// Return whether this invocation has been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Mark this invocation as cancelled.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

/// Serializable cancellation/deadline metadata visible to plugins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCancellationToken {
    /// Invocation identifier shared by request, response, event, and control frames.
    #[serde(default)]
    pub invocation_id: PluginInvocationId,
    /// Whether the invocation was cancelled before or during dispatch.
    #[serde(default)]
    pub cancelled: bool,
    /// Optional deadline expressed as milliseconds from the host invocation start.
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}

impl Default for PluginCancellationToken {
    fn default() -> Self {
        Self::none()
    }
}

impl PluginCancellationToken {
    /// Construct a non-cancelled token with a fresh invocation id and no deadline.
    #[must_use]
    pub fn new() -> Self {
        Self::none()
    }

    /// Construct a non-cancelled token with a fresh invocation id and no deadline.
    #[must_use]
    pub fn none() -> Self {
        Self {
            invocation_id: PluginInvocationId::new(),
            cancelled: false,
            deadline_ms: None,
        }
    }

    /// Construct a non-cancelled token with a fresh invocation id and no deadline.
    #[must_use]
    pub fn active() -> Self {
        Self::none()
    }

    /// Construct a cancelled token with a fresh invocation id.
    #[must_use]
    pub fn cancelled() -> Self {
        let mut token = Self::none();
        token.cancel();
        token
    }

    /// Construct a non-cancelled token with a relative deadline.
    #[must_use]
    pub fn with_deadline(deadline: Duration) -> Self {
        let deadline_ms = u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX);
        Self {
            invocation_id: PluginInvocationId::new(),
            cancelled: false,
            deadline_ms: Some(deadline_ms),
        }
    }

    /// Return whether this invocation has been cancelled.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    /// Mark this serializable token snapshot as cancelled.
    pub const fn cancel(&mut self) {
        self.cancelled = true;
    }
}

/// Backward-compatible name for serializable plugin cancellation metadata.
pub type CancellationToken = PluginCancellationToken;
