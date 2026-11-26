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

fn encode_invocation_id(bytes: [u8; INVOCATION_ID_LEN]) -> String {
    // A power-of-two alphabet makes masking uniform without rejection sampling.
    bytes
        .into_iter()
        .map(|byte| INVOCATION_ID_ALPHABET[usize::from(byte & 63)])
        .collect()
}

/// Serializable identifier that correlates all frames for one plugin invocation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginInvocationId(String);

impl PluginInvocationId {
    /// Create a new 21-character invocation identifier using Switchy's secure randomness.
    ///
    /// Switchy's simulator feature substitutes deterministic randomness for replay.
    /// Production builds must not enable that feature.
    ///
    /// # Panics
    ///
    /// Panics if the random source fails; no weaker fallback is used.
    #[must_use]
    pub fn new() -> Self {
        let mut bytes = [0; INVOCATION_ID_LEN];
        switchy::random::secure_fill_bytes(&mut bytes)
            .expect("failed to generate plugin invocation ID randomness");
        Self(encode_invocation_id(bytes))
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

    /// How long the caller still waits for this invocation's reply, from
    /// when the host started it; `None` when the caller set no deadline.
    /// Waits inside a handler should end by then, answering
    /// [`crate::ServiceResponse::deadline_exceeded`]. The host also signals
    /// cancellation when it passes.
    #[must_use]
    pub const fn deadline(&self) -> Option<Duration> {
        match self.deadline_ms {
            Some(milliseconds) => Some(Duration::from_millis(milliseconds)),
            None => None,
        }
    }

    /// This token for a caller that stops waiting `remaining` from now.
    /// A deadline under a millisecond rounds up: zero means already expired.
    #[must_use]
    pub fn with_remaining(mut self, remaining: Option<Duration>) -> Self {
        self.deadline_ms = remaining.map(|remaining| {
            u64::try_from(remaining.as_micros().div_ceil(1000)).unwrap_or(u64::MAX)
        });
        self
    }

    /// Mark this serializable token snapshot as cancelled.
    pub const fn cancel(&mut self) {
        self.cancelled = true;
    }
}

/// Backward-compatible name for serializable plugin cancellation metadata.
pub type CancellationToken = PluginCancellationToken;

#[cfg(test)]
mod tests {
    use super::{PluginCancellationToken, PluginInvocationId, ServiceCancellation};
    use std::time::Duration;

    #[test]
    fn invocation_id_encoding_preserves_alphabet_and_masks_uniformly() {
        for byte in 0..=u8::MAX {
            let encoded = super::encode_invocation_id([byte; super::INVOCATION_ID_LEN]);
            let expected = super::INVOCATION_ID_ALPHABET[usize::from(byte & 63)];
            assert_eq!(
                encoded,
                expected.to_string().repeat(super::INVOCATION_ID_LEN)
            );
        }
    }

    #[test]
    fn invocation_id_serialization_remains_a_string() {
        let encoded = "0123456789abcdefghijk";
        let json = format!("\"{encoded}\"");
        let id: PluginInvocationId = serde_json::from_str(&json).unwrap();
        assert_eq!(id.as_str(), encoded);
        assert_eq!(serde_json::to_string(&id).unwrap(), json);
    }

    #[test]
    fn invocation_ids_preserve_shape_and_are_unique() {
        let first = PluginInvocationId::new();
        let second = PluginInvocationId::new();
        assert_eq!(first.as_str().len(), 21);
        assert_ne!(first, second);
        assert!(
            first
                .as_str()
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        );
    }

    #[test]
    fn shared_service_cancellation_updates_all_clones() {
        let cancellation = ServiceCancellation::new();
        let cloned = cancellation.clone();
        assert!(!cancellation.is_cancelled());
        cloned.cancel();
        assert!(cancellation.is_cancelled());
        assert!(cloned.is_cancelled());
    }

    #[test]
    fn cancellation_token_defaults_to_active_with_invocation_id() {
        let token = PluginCancellationToken::default();
        assert!(!token.is_cancelled());
        assert_eq!(token.invocation_id.as_str().len(), 21);
        assert_eq!(token.deadline_ms, None);
    }

    #[test]
    fn cancellation_token_carries_cancelled_and_deadline_metadata() {
        let cancelled = PluginCancellationToken::cancelled();
        assert!(cancelled.is_cancelled());

        let deadline = PluginCancellationToken::with_deadline(Duration::from_millis(42));
        assert!(!deadline.is_cancelled());
        assert_eq!(deadline.deadline_ms, Some(42));
        assert_eq!(deadline.deadline(), Some(Duration::from_millis(42)));
        let rounded =
            PluginCancellationToken::none().with_remaining(Some(Duration::from_micros(1)));
        assert_eq!(rounded.deadline(), Some(Duration::from_millis(1)));
        assert_eq!(
            PluginCancellationToken::none()
                .with_remaining(None)
                .deadline(),
            None
        );
    }
}
