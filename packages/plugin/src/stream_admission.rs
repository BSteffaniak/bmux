//! Neutral, host-trusted ingress for already authenticated streams.
//!
//! A transport (for example a plugin's network listener) authenticates its
//! peer itself, creates a [`StreamAdmissionBinding`] naming the principal, and
//! hands the stream to the server through the [`StreamIngressHandle`] the
//! server registered in [`crate::PluginStateRegistry`]. The server serves the
//! stream as an ordinary connection whose provenance is
//! [`StreamProvenance::Admitted`]. The transport keeps the [`StreamRevocation`]
//! and calls [`StreamRevocation::revoke`] to end every connection using that
//! binding at once; nothing is polled.
//!
//! Bindings are created only by in-process host code, never from wire data.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bmux_ipc::transport::ErasedIpcStream;
use tokio::sync::Notify;
use uuid::Uuid;

#[derive(Debug, Default)]
struct RevocationState {
    revoked: AtomicBool,
    notify: Notify,
}

/// Immutable identity of one remote stream, created only by in-process host code.
#[derive(Debug)]
pub struct StreamAdmissionBinding {
    subject_id: Uuid,
    connection_id: Uuid,
    revocation: Arc<RevocationState>,
}

impl StreamAdmissionBinding {
    /// Returns the binding and the only handle that can revoke it.
    ///
    /// `subject_id` is the authenticated principal; `connection_id` names the
    /// transport connection the binding belongs to (several streams of one
    /// connection may share a binding).
    #[must_use]
    pub fn new(subject_id: Uuid, connection_id: Uuid) -> (Arc<Self>, StreamRevocation) {
        let revocation = Arc::new(RevocationState::default());
        (
            Arc::new(Self {
                subject_id,
                connection_id,
                revocation: Arc::clone(&revocation),
            }),
            StreamRevocation { state: revocation },
        )
    }

    /// The authenticated principal this stream acts as.
    #[must_use]
    pub const fn subject_id(&self) -> Uuid {
        self.subject_id
    }

    /// The transport connection this binding was issued for.
    #[must_use]
    pub const fn connection_id(&self) -> Uuid {
        self.connection_id
    }

    /// Whether [`StreamRevocation::revoke`] has been called.
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.revocation.revoked.load(Ordering::Acquire)
    }

    /// Resolves once revoked (immediately if already revoked). Cancel-safe:
    /// dropping the future loses nothing, and a new one observes the same state.
    pub async fn revoked(&self) {
        loop {
            let mut notified = std::pin::pin!(self.revocation.notify.notified());
            // Register before checking so a concurrent revoke cannot be missed.
            notified.as_mut().enable();
            if self.is_revoked() {
                return;
            }
            notified.await;
        }
    }
}

/// Cloneable; revoking is idempotent and wakes every `revoked()` waiter.
#[derive(Debug, Clone)]
pub struct StreamRevocation {
    state: Arc<RevocationState>,
}

impl StreamRevocation {
    /// Revoke the binding. Every connection using it ends; repeated calls do nothing.
    pub fn revoke(&self) {
        if !self.state.revoked.swap(true, Ordering::AcqRel) {
            self.state.notify.notify_waiters();
        }
    }

    /// Whether this revocation has been triggered.
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.state.revoked.load(Ordering::Acquire)
    }
}

/// Why the server refused an admitted stream. The stream is dropped (closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StreamIngressError {
    /// The server is at its connection limit; it fails closed.
    #[error("server is at its connection limit")]
    AtCapacity,
    /// The server is shutting down or no longer running.
    #[error("server is not accepting connections")]
    Unavailable,
    /// The binding was already revoked.
    #[error("stream binding is revoked")]
    Revoked,
}

/// Server side of [`StreamIngressHandle`]. Implemented by the server only.
pub trait StreamIngress: Send + Sync {
    /// See [`StreamIngressHandle::accept`].
    ///
    /// # Errors
    /// See [`StreamIngressError`].
    fn accept(
        &self,
        stream: ErasedIpcStream,
        binding: Arc<StreamAdmissionBinding>,
    ) -> Result<(), StreamIngressError>;
}

/// Registered in `PluginStateRegistry` by the server before plugins activate.
#[derive(Clone)]
pub struct StreamIngressHandle {
    ingress: Arc<dyn StreamIngress>,
}

impl std::fmt::Debug for StreamIngressHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StreamIngressHandle(..)")
    }
}

impl StreamIngressHandle {
    /// Wrap the server's ingress implementation.
    #[must_use]
    pub fn new(ingress: Arc<dyn StreamIngress>) -> Self {
        Self { ingress }
    }

    /// Resolve the handle the server registered, if any.
    #[must_use]
    pub fn from_host_registry(registry: &crate::PluginStateRegistry) -> Option<Self> {
        registry
            .get::<Self>()?
            .read()
            .ok()
            .map(|handle| handle.clone())
    }

    /// Serve `stream` as one connection whose provenance is `Admitted(binding)`.
    /// The server applies its normal connection limits; a full server fails
    /// closed. The connection ends when the stream ends or the binding is revoked.
    ///
    /// # Errors
    /// Returns an error (and drops the stream) when the server refuses it.
    pub fn accept(
        &self,
        stream: ErasedIpcStream,
        binding: Arc<StreamAdmissionBinding>,
    ) -> Result<(), StreamIngressError> {
        if binding.is_revoked() {
            return Err(StreamIngressError::Revoked);
        }
        self.ingress.accept(stream, binding)
    }
}

/// Provenance bookkeeping failures; they expose no details.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("stream provenance rejected")]
pub struct StreamAdmissionError;

/// Positively recorded host provenance. Unknown and revoked streams are invalid.
pub enum StreamProvenance {
    Local,
    Admitted(Arc<StreamAdmissionBinding>),
    Invalid,
}

enum RecordedOrigin {
    Local,
    Admitted(Arc<StreamAdmissionBinding>),
}

/// Neutral host-only origin bookkeeping. Never populate from request metadata.
#[derive(Clone, Default)]
pub struct StreamProvenanceRegistry {
    origins: Arc<Mutex<BTreeMap<Uuid, RecordedOrigin>>>,
}

impl StreamProvenanceRegistry {
    /// Resolve only from the actual host's explicitly supplied registry.
    /// A library-local global registry is NOT a substitute across cdylib or
    /// process boundaries. Those runtimes need an explicit generic host-context
    /// bridge; until supplied, consumers must deny rather than infer locality.
    #[must_use]
    pub fn from_host_registry(registry: &crate::PluginStateRegistry) -> Option<Self> {
        registry
            .get::<Self>()?
            .read()
            .ok()
            .map(|handle| handle.clone())
    }

    /// Resolve a host-generated stream UUID. A revoked binding is `Invalid`.
    #[must_use]
    pub fn lookup(&self, stream_id: Uuid) -> StreamProvenance {
        let Ok(origins) = self.origins.lock() else {
            return StreamProvenance::Invalid;
        };
        match origins.get(&stream_id) {
            Some(RecordedOrigin::Local) => StreamProvenance::Local,
            Some(RecordedOrigin::Admitted(binding)) if !binding.is_revoked() => {
                StreamProvenance::Admitted(Arc::clone(binding))
            }
            Some(RecordedOrigin::Admitted(_)) | None => StreamProvenance::Invalid,
        }
    }

    /// Host-only: record a connection's origin, never a wire claim.
    /// `None` is permitted only for an ordinary direct local stream.
    ///
    /// # Errors
    /// Rejects duplicate identities, poisoned bookkeeping, or revoked bindings.
    pub fn record(
        &self,
        stream_id: Uuid,
        admitted: Option<Arc<StreamAdmissionBinding>>,
    ) -> Result<StreamProvenanceGuard, StreamAdmissionError> {
        let origin = match admitted {
            Some(binding) if binding.is_revoked() => return Err(StreamAdmissionError),
            Some(binding) => RecordedOrigin::Admitted(binding),
            None => RecordedOrigin::Local,
        };
        let mut origins = self.origins.lock().map_err(|_| StreamAdmissionError)?;
        if origins.contains_key(&stream_id) {
            return Err(StreamAdmissionError);
        }
        origins.insert(stream_id, origin);
        drop(origins);
        Ok(StreamProvenanceGuard {
            registry: self.clone(),
            stream_id,
        })
    }
}

/// Host connection lifetime; removal always makes provenance invalid, not local.
pub struct StreamProvenanceGuard {
    registry: StreamProvenanceRegistry,
    stream_id: Uuid,
}

impl Drop for StreamProvenanceGuard {
    fn drop(&mut self) {
        if let Ok(mut origins) = self.registry.origins.lock() {
            origins.remove(&self.stream_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn revoke_wakes_every_waiter_and_is_idempotent() {
        let (binding, revocation) =
            StreamAdmissionBinding::new(Uuid::from_u128(1), Uuid::from_u128(2));
        assert_eq!(binding.subject_id(), Uuid::from_u128(1));
        assert_eq!(binding.connection_id(), Uuid::from_u128(2));
        assert!(!binding.is_revoked());
        let waiters = (0..4)
            .map(|_| {
                let binding = Arc::clone(&binding);
                tokio::spawn(async move { binding.revoked().await })
            })
            .collect::<Vec<_>>();
        tokio::task::yield_now().await;
        let clone = revocation.clone();
        revocation.revoke();
        clone.revoke();
        for waiter in waiters {
            tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .expect("waiter woken")
                .unwrap();
        }
        assert!(binding.is_revoked());
        // Already revoked: resolves immediately.
        tokio::time::timeout(Duration::from_millis(100), binding.revoked())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn revoked_future_is_cancel_safe() {
        let (binding, revocation) = StreamAdmissionBinding::new(Uuid::new_v4(), Uuid::new_v4());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), binding.revoked())
                .await
                .is_err()
        );
        revocation.revoke();
        tokio::time::timeout(Duration::from_millis(100), binding.revoked())
            .await
            .unwrap();
    }

    #[test]
    fn provenance_registry_tracks_revocation_and_guard_lifetime() {
        let registry = StreamProvenanceRegistry::default();
        let (binding, revocation) = StreamAdmissionBinding::new(Uuid::new_v4(), Uuid::new_v4());
        let local = Uuid::new_v4();
        let admitted = Uuid::new_v4();
        let local_guard = registry.record(local, None).unwrap();
        let admitted_guard = registry
            .record(admitted, Some(Arc::clone(&binding)))
            .unwrap();
        assert!(registry.record(local, None).is_err());
        assert!(matches!(registry.lookup(local), StreamProvenance::Local));
        assert!(matches!(
            registry.lookup(admitted),
            StreamProvenance::Admitted(found) if Arc::ptr_eq(&found, &binding)
        ));
        revocation.revoke();
        assert!(matches!(
            registry.lookup(admitted),
            StreamProvenance::Invalid
        ));
        assert!(
            registry
                .record(Uuid::new_v4(), Some(Arc::clone(&binding)))
                .is_err()
        );
        drop(local_guard);
        drop(admitted_guard);
        assert!(matches!(registry.lookup(local), StreamProvenance::Invalid));
    }
}
