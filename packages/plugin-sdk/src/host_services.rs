/// Host-selected independent retained capability opener. The callback must open
/// the selected activation's actual ledger, not adapt invocation-local dispatch.
pub type RetainedHostOpener =
    dyn Fn() -> crate::Result<crate::abi::retained::RetainedHostPort> + Send + Sync;

/// Owned future returned by a selected asynchronous host service callback.
pub type HostServiceFuture = crate::definition::PluginFuture<'static, crate::Result<Vec<u8>>>;

/// Owned operation data for an explicitly asynchronous host callback.
/// Identity fields name an operation, never caller authority or a worker grant.
#[derive(Debug, Clone)]
pub struct HostServiceOwnedRequest {
    pub capability: String,
    pub kind: crate::ServiceKind,
    pub interface: String,
    pub operation: String,
    pub payload: Vec<u8>,
}

impl HostServiceOwnedRequest {
    /// Borrow for synchronous execution inside a trusted host worker.
    #[must_use]
    pub fn as_request(&self) -> crate::HostServiceRequest<'_> {
        crate::HostServiceRequest {
            capability: &self.capability,
            kind: self.kind,
            interface: &self.interface,
            operation: &self.operation,
            payload: &self.payload,
        }
    }

    /// Check bounds before retaining or scheduling the operation.
    ///
    /// # Errors
    /// Rejects oversized payloads or empty/oversized operation identifiers.
    pub fn validate(&self) -> crate::Result<()> {
        if self.payload.len() > crate::abi::MAX_BUFFER_BYTES
            || [&self.capability, &self.interface, &self.operation]
                .iter()
                .any(|value| {
                    value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control)
                })
        {
            return Err(crate::PluginError::ServiceProtocol {
                details: "async host request exceeds bounds or has invalid identifiers".into(),
            });
        }
        Ok(())
    }
}

impl From<crate::HostServiceRequest<'_>> for HostServiceOwnedRequest {
    fn from(request: crate::HostServiceRequest<'_>) -> Self {
        Self {
            capability: request.capability.into(),
            kind: request.kind,
            interface: request.interface.into(),
            operation: request.operation.into(),
            payload: request.payload.into(),
        }
    }
}

use std::{cell::RefCell, collections::BTreeMap, fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Generic plugin command outcome metadata key for a concise user-facing
/// status message.
pub const COMMAND_OUTCOME_STATUS_MESSAGE_KEY: &str = "bmux.status_message";

/// Return true when `key` is a valid host storage key.
///
/// Storage keys are intentionally path-segment-like identifiers so every host
/// can safely map them to files or key-value stores without escaping. They must
/// be non-empty and contain only ASCII letters, digits, `.`, `_`, or `-`.
#[must_use]
pub const fn storage_key_is_valid(key: &str) -> bool {
    let bytes = key.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        let valid = byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-');
        if !valid {
            return false;
        }
        index += 1;
    }
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageKeyError {
    key: String,
}

impl StorageKeyError {
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl fmt::Display for StorageKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "storage key {:?} is invalid; use non-empty [A-Za-z0-9._-]",
            self.key
        )
    }
}

impl std::error::Error for StorageKeyError {}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StorageKey(String);

impl StorageKey {
    /// Construct a validated storage key.
    ///
    /// # Errors
    ///
    /// Returns [`StorageKeyError`] when `key` is empty or contains characters
    /// outside `[A-Za-z0-9._-]`.
    pub fn new(key: impl Into<String>) -> Result<Self, StorageKeyError> {
        let key = key.into();
        if storage_key_is_valid(&key) {
            Ok(Self(key))
        } else {
            Err(StorageKeyError { key })
        }
    }

    /// Construct a key from a literal already validated by [`storage_key!`].
    ///
    /// Prefer the macro for literals and [`Self::new`] for dynamic input.
    #[doc(hidden)]
    #[must_use]
    pub fn from_validated_literal(key: &'static str) -> Self {
        debug_assert!(storage_key_is_valid(key));
        Self(key.to_string())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for StorageKey {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for StorageKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for StorageKey {
    type Err = StorageKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for StorageKey {
    type Error = StorageKeyError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for StorageKey {
    type Error = StorageKeyError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for StorageKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for StorageKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let key = String::deserialize(deserializer)?;
        Self::new(key).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageGetRequest {
    pub key: StorageKey,
}

impl StorageGetRequest {
    #[must_use]
    pub const fn new(key: StorageKey) -> Self {
        Self { key }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageGetResponse {
    pub value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageSetRequest {
    pub key: StorageKey,
    pub value: Vec<u8>,
}

impl StorageSetRequest {
    #[must_use]
    pub fn new(key: StorageKey, value: impl Into<Vec<u8>>) -> Self {
        Self {
            key,
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolatileStateGetRequest {
    pub key: StorageKey,
}

impl VolatileStateGetRequest {
    #[must_use]
    pub const fn new(key: StorageKey) -> Self {
        Self { key }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolatileStateGetResponse {
    pub value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolatileStateSetRequest {
    pub key: StorageKey,
    pub value: Vec<u8>,
}

impl VolatileStateSetRequest {
    #[must_use]
    pub fn new(key: StorageKey, value: impl Into<Vec<u8>>) -> Self {
        Self {
            key,
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolatileStateClearRequest {
    pub key: StorageKey,
}

impl VolatileStateClearRequest {
    #[must_use]
    pub const fn new(key: StorageKey) -> Self {
        Self { key }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogWriteLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogWriteRequest {
    pub level: LogWriteLevel,
    pub message: String,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCommandOutcome {
    /// Error message from the plugin command's `Err` return, if any.
    ///
    /// Populated by the SDK's FFI boundary when a `RustPlugin`
    /// command returns `Err(PluginCommandError)`. Hosts use this to
    /// log the error and render a user-facing indicator, instead of
    /// relying on the plugin's stderr (which would corrupt attach
    /// TTYs for in-process plugins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Generic metadata produced by a plugin command.
    ///
    /// Hosts may interpret well-known keys for their own runtime surfaces while
    /// the SDK remains domain-agnostic.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

thread_local! {
    static COMMAND_OUTCOME_CAPTURE: RefCell<Option<PluginCommandOutcome>> = const { RefCell::new(None) };
}

#[doc(hidden)]
pub fn begin_command_outcome_capture() {
    COMMAND_OUTCOME_CAPTURE.with(|slot| {
        *slot.borrow_mut() = Some(PluginCommandOutcome::default());
    });
}

/// Run `body` with a fresh capture active on this thread and return its
/// result with the metadata it recorded. Restores any enclosing capture.
///
/// This is the plugin-side half of the explicit command outcome path: the
/// captured map becomes part of [`crate::definition::CommandOutcome`], which
/// the shared ABI returns as data.
pub fn capture_command_outcome_metadata<T>(
    body: impl FnOnce() -> T,
) -> (T, BTreeMap<String, Value>) {
    struct Restore(Option<PluginCommandOutcome>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            COMMAND_OUTCOME_CAPTURE.with(|slot| *slot.borrow_mut() = previous);
        }
    }
    let restore = Restore(
        COMMAND_OUTCOME_CAPTURE
            .with(|slot| slot.borrow_mut().replace(PluginCommandOutcome::default())),
    );
    let result = body();
    let metadata = COMMAND_OUTCOME_CAPTURE
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or_default()
        .metadata;
    drop(restore);
    (result, metadata)
}

#[doc(hidden)]
#[must_use]
pub fn finish_command_outcome_capture() -> PluginCommandOutcome {
    COMMAND_OUTCOME_CAPTURE
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or_default()
}

/// Record metadata for the currently-running plugin command.
///
/// If no command capture is active, this is a no-op. That keeps plugin code safe
/// to call from CLI, service, and test harnesses that do not collect outcomes.
pub fn record_command_outcome_metadata(key: impl Into<String>, value: Value) {
    COMMAND_OUTCOME_CAPTURE.with(|slot| {
        if let Some(outcome) = slot.borrow_mut().as_mut() {
            outcome.metadata.insert(key.into(), value);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_key_accepts_safe_identifiers() {
        for key in [
            "selected_theme",
            "theme_settings.performance",
            "cluster.pane.abc-123",
        ] {
            let parsed = StorageKey::new(key).expect("valid storage key should parse");
            assert_eq!(parsed.as_str(), key);
            assert_eq!(parsed.to_string(), key);
        }
    }

    #[test]
    fn storage_key_rejects_unsafe_identifiers() {
        for key in ["", "theme:settings", "path/key", "has space", "emoji-🚀"] {
            assert!(StorageKey::new(key).is_err(), "{key:?} should be invalid");
            assert!(!storage_key_is_valid(key), "{key:?} should be invalid");
        }
    }

    #[test]
    fn storage_key_round_trips_as_json_string() {
        let key = StorageKey::new("theme_settings.performance").expect("valid key");
        let encoded = serde_json::to_string(&key).expect("key should serialize");
        assert_eq!(encoded, "\"theme_settings.performance\"");
        let decoded: StorageKey = serde_json::from_str(&encoded).expect("key should deserialize");
        assert_eq!(decoded, key);
    }

    #[test]
    fn storage_key_rejects_invalid_json_string() {
        let error = serde_json::from_str::<StorageKey>("\"theme:settings\"")
            .expect_err("invalid key should not deserialize");
        assert!(error.to_string().contains("storage key"));
    }

    #[test]
    fn storage_requests_round_trip_with_valid_keys() {
        let set = StorageSetRequest::new(
            crate::storage_key!("selected_theme"),
            b"performance".to_vec(),
        );
        let encoded = serde_json::to_string(&set).expect("request should serialize");
        assert!(encoded.contains("\"key\":\"selected_theme\""));
        let decoded: StorageSetRequest =
            serde_json::from_str(&encoded).expect("request should deserialize");
        assert_eq!(decoded.key.as_str(), "selected_theme");
        assert_eq!(decoded.value, b"performance");

        let get = StorageGetRequest::new(crate::storage_key!("selected_theme"));
        let encoded = serde_json::to_string(&get).expect("request should serialize");
        let decoded: StorageGetRequest =
            serde_json::from_str(&encoded).expect("request should deserialize");
        assert_eq!(decoded.key.as_str(), "selected_theme");
    }

    #[test]
    fn storage_requests_reject_invalid_wire_keys() {
        let error = serde_json::from_str::<StorageGetRequest>(r#"{"key":"bad:key"}"#)
            .expect_err("invalid request key should fail");
        assert!(error.to_string().contains("storage key"));
    }
}

#[cfg(test)]
mod async_host_tests {
    struct RetainedLedger;
    impl crate::abi::retained::RetainedHostDispatch for RetainedLedger {
        fn begin(
            &self,
            _: u64,
            _: &crate::HostServiceRequest<'_>,
            _: crate::abi::retained::NativeRetainedReservationV1,
        ) -> crate::Result<u64> {
            Ok(1)
        }
        fn observe(
            &self,
            _: u64,
            _: u64,
        ) -> crate::Result<crate::abi::retained::RetainedHostObservation> {
            Err(crate::PluginError::UnsupportedHostOperation {
                operation: "test_observe",
            })
        }
        fn control(&self, _: u64, _: u32) -> crate::Result<bool> {
            Ok(true)
        }
        fn acknowledge(&self, _: u64, _: u64) -> crate::Result<()> {
            Ok(())
        }
        fn release(&self) -> crate::Result<bool> {
            Ok(true)
        }
    }

    #[test]
    fn retained_host_is_explicit_and_preserved_by_other_capability_rebindings() {
        let services = PluginHostServices::new(|_| panic!("retained open must not invoke service"));
        assert!(matches!(
            services.open_retained_host(),
            Err(crate::PluginError::UnsupportedHostOperation {
                operation: "open_retained_host"
            })
        ));
        let opens = Arc::new(AtomicUsize::new(0));
        let seen = opens.clone();
        let bound = services.clone().with_retained_host(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(crate::abi::retained::RetainedHostPort::new(Arc::new(
                RetainedLedger,
            )))
        });
        assert_ne!(services, bound);
        assert_eq!(bound, bound.clone());
        let decorated = bound
            .clone()
            .with_kernel(|_| Ok(Vec::new()))
            .with_async(|_| Box::pin(async { Ok(Vec::new()) }))
            .with_registrations(|_| Ok(Vec::new()));
        assert!(decorated.open_retained_host().unwrap().release().unwrap());
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        let failure = bound.with_retained_host(|| {
            Err(crate::PluginError::ServiceProtocol {
                details: "retained owner closed".into(),
            })
        });
        assert!(
            failure
                .open_retained_host()
                .err()
                .unwrap()
                .to_string()
                .contains("retained owner closed")
        );
        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }

    use crate::{HostServiceOwnedRequest, PluginHostServices, ServiceKind};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn request() -> HostServiceOwnedRequest {
        HostServiceOwnedRequest {
            capability: "example".into(),
            kind: ServiceKind::Query,
            interface: "example/v1".into(),
            operation: "get".into(),
            payload: vec![1],
        }
    }

    #[tokio::test]
    async fn async_capability_is_optional_and_never_invokes_sync_fallback() {
        let services = PluginHostServices::new(|_| panic!("no sync fallback"));
        assert!(services.call_service_async(request()).await.is_err());
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let asynchronous = services.clone().with_async(move |request| {
            let seen = seen.clone();
            Box::pin(async move {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(request.payload)
            })
        });
        assert_ne!(services, asynchronous);
        let future = asynchronous.call_service_async(request());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(future.await.unwrap(), vec![1]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn async_bounds_precede_dispatch_and_oversized_responses_do_not_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let services =
            PluginHostServices::new(|_| panic!("no sync fallback")).with_async(move |_| {
                let seen = seen.clone();
                Box::pin(async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Ok(vec![0; crate::abi::MAX_BUFFER_BYTES + 1])
                })
            });
        let mut oversized = request();
        oversized.payload = vec![0; crate::abi::MAX_BUFFER_BYTES + 1];
        assert!(services.call_service_async(oversized).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(services.call_service_async(request()).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
