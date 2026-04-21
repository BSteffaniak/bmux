//! `WireEventSink` — plugin-side boundary for publishing wire events.
//!
//! Wire events (`bmux_ipc::Event`) are the cross-process, protocol-stable
//! event stream that streaming/attach clients subscribe to. Historically
//! the server translated typed plugin events into wire events through
//! per-plugin bridges (`spawn_control_catalog_bridge`,
//! `spawn_client_events_bridge`, `spawn_performance_events_bridge`).
//!
//! This trait reverses that flow: plugins that need to publish on the
//! wire obtain a [`WireEventSinkHandle`] from the shared plugin state
//! registry and call [`WireEventSink::publish`] directly. The server
//! registers its concrete sink at startup. When the server is absent
//! (e.g. in unit tests), [`NoopWireEventSink`] swallows every publish.
//!
//! The trait is domain-agnostic: it carries an opaque
//! [`bmux_ipc::Event`], and it makes no assumption about which events
//! are valid or who publishes which kind. Capability gating and
//! per-plugin event ownership are expressed elsewhere (plugin
//! manifest + BPDL schemas).

use std::sync::Arc;

use bmux_ipc::Event;

/// Sink for cross-process wire events published by plugins.
///
/// Implementations must be cheap to `clone` (the server stores one
/// `Arc<dyn WireEventSink>` behind a handle and shares it across every
/// plugin's activation path). `publish` is expected to be non-blocking
/// and failure-tolerant: events dropped by a saturated broadcast
/// channel are not re-delivered.
pub trait WireEventSink: Send + Sync {
    /// Publish `event` to every streaming/attach subscriber.
    ///
    /// Implementations should return `Ok(())` even when there are no
    /// receivers. Genuine errors (e.g. poisoned mutex, serialization
    /// failure on a downstream bridge) are surfaced via the `Result`
    /// so the caller can log and continue.
    ///
    /// # Errors
    ///
    /// Returns [`WireEventSinkError::PublishFailed`] when the sink
    /// encounters an internal failure (lock poison, queue saturation,
    /// transport error) that prevents delivering to all subscribers.
    fn publish(&self, event: Event) -> Result<(), WireEventSinkError>;
}

/// Errors returned by [`WireEventSink::publish`].
#[derive(Debug, thiserror::Error)]
pub enum WireEventSinkError {
    /// The sink encountered an internal failure (lock poison, queue
    /// saturation, transport error). The event was not delivered to
    /// all subscribers.
    #[error("wire event sink failed to publish: {0}")]
    PublishFailed(String),
}

/// Registry newtype wrapping an `Arc<dyn WireEventSink>`.
///
/// Plugins look this up in the shared plugin state registry and clone
/// it into their own state so they can publish without touching the
/// host directly.
#[derive(Clone)]
pub struct WireEventSinkHandle(pub Arc<dyn WireEventSink>);

impl WireEventSinkHandle {
    #[must_use]
    pub fn new<S: WireEventSink + 'static>(sink: S) -> Self {
        Self(Arc::new(sink))
    }

    #[must_use]
    pub fn from_arc(sink: Arc<dyn WireEventSink>) -> Self {
        Self(sink)
    }

    /// Construct a sink handle that swallows every publish.
    ///
    /// Registered by the SDK at startup so plugins can unconditionally
    /// resolve a handle even when no server is attached (tests,
    /// headless tooling).
    #[must_use]
    pub fn noop() -> Self {
        Self::new(NoopWireEventSink)
    }

    /// Borrow the underlying trait object.
    #[must_use]
    pub fn as_dyn(&self) -> &(dyn WireEventSink + 'static) {
        self.0.as_ref()
    }
}

/// No-op default impl. `publish` silently succeeds.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopWireEventSink;

impl WireEventSink for NoopWireEventSink {
    fn publish(&self, _event: Event) -> Result<(), WireEventSinkError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct CapturingSink {
        events: Mutex<Vec<Event>>,
    }

    impl WireEventSink for CapturingSink {
        fn publish(&self, event: Event) -> Result<(), WireEventSinkError> {
            self.events
                .lock()
                .map_err(|_| WireEventSinkError::PublishFailed("poisoned".into()))?
                .push(event);
            Ok(())
        }
    }

    #[test]
    fn noop_sink_accepts_any_event() {
        let sink = NoopWireEventSink;
        assert!(sink.publish(Event::ServerStarted).is_ok());
        assert!(sink.publish(Event::ServerStopping).is_ok());
    }

    #[test]
    fn handle_clone_shares_inner_sink() {
        let capturing = CapturingSink {
            events: Mutex::new(Vec::new()),
        };
        let handle = WireEventSinkHandle::new(capturing);
        let clone = handle.clone();
        clone
            .as_dyn()
            .publish(Event::ServerStarted)
            .expect("publish");
        handle
            .as_dyn()
            .publish(Event::ServerStopping)
            .expect("publish");
        // Both clones pointed at the same sink; we can't introspect
        // CapturingSink through the trait, but the Arc identity is
        // preserved by construction.
        assert_eq!(Arc::strong_count(&handle.0), 2);
    }

    #[test]
    fn noop_handle_constructs_and_publishes() {
        let handle = WireEventSinkHandle::noop();
        handle
            .as_dyn()
            .publish(Event::ServerStarted)
            .expect("noop publish");
    }
}
