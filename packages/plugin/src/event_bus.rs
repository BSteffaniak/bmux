//! Host-side typed event bus keyed by `PluginEventKind`.
//!
//! Plugins register a broadcast channel for each BPDL `events <T>;`
//! interface they own, keyed by the BPDL-generated `EVENT_KIND`
//! constant (a [`PluginEventKind`] — the canonical
//! `<plugin-id>/<interface-name>` tuple). Consumers (other plugins,
//! the server, or the attach CLI via an IPC bridge) obtain typed
//! receivers by looking up the same kind.
//!
//! The bus holds `tokio::sync::broadcast::Sender<Arc<dyn Any + Send +
//! Sync>>` values behind a type-erased handle; at subscribe time we
//! downcast back to the concrete payload type. In-process senders and
//! subscribers pay zero serialization cost.
//!
//! # Lifecycle
//!
//! 1. Plugin's `activate` calls
//!    [`EventBus::register_channel::<E>`] for each event payload type
//!    it owns, using the BPDL-generated `EVENT_KIND` constant.
//! 2. Publishers call [`EventBus::emit::<E>`]; the payload is
//!    `Arc`-wrapped and sent to every live subscriber.
//! 3. Subscribers call [`EventBus::subscribe::<E>`] to obtain a
//!    typed `Receiver<Arc<E>>`.

use bmux_plugin_sdk::PluginEventKind;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;

/// Default capacity for newly-registered broadcast channels.
///
/// Plugins can override via [`EventBus::register_channel_with_capacity`].
pub const DEFAULT_EVENT_BUS_CAPACITY: usize = 1024;

/// Error variants the event bus can produce.
#[derive(Debug)]
pub enum EventBusError {
    /// Attempted to emit or subscribe on an interface that no plugin
    /// has registered a channel for.
    ChannelNotRegistered {
        /// The interface id that was queried.
        interface: String,
    },
    /// The stored channel's payload type did not match the caller's
    /// expected type. Indicates a registration/consumer mismatch.
    PayloadTypeMismatch {
        /// The interface id involved.
        interface: String,
        /// The type name the registered channel holds.
        expected: &'static str,
        /// The type name the caller asked for.
        actual: &'static str,
    },
}

impl std::fmt::Display for EventBusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChannelNotRegistered { interface } => {
                write!(f, "no event channel registered for interface `{interface}`")
            }
            Self::PayloadTypeMismatch {
                interface,
                expected,
                actual,
            } => write!(
                f,
                "event channel for `{interface}` has payload type `{expected}`; \
                 caller requested `{actual}`"
            ),
        }
    }
}

impl std::error::Error for EventBusError {}

/// Result alias for event bus operations.
pub type EventBusResult<T> = std::result::Result<T, EventBusError>;

/// Internal handle stored behind an `Arc<dyn Any>`, giving the bus a
/// uniform type to key on even though each channel's payload type
/// differs.
struct ChannelEntry {
    sender: Arc<dyn Any + Send + Sync>,
    payload_type_id: TypeId,
    payload_type_name: &'static str,
}

/// Host-side typed event bus.
#[derive(Default)]
pub struct EventBus {
    entries: RwLock<HashMap<PluginEventKind, ChannelEntry>>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.entries.read().map_or(0, |g| g.len());
        f.debug_struct("EventBus")
            .field("channels", &count)
            .finish()
    }
}

impl EventBus {
    /// Construct an empty event bus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a broadcast channel for `E` keyed by `interface`.
    ///
    /// Returns the `Sender` so the registering plugin can keep a
    /// handle for direct publishing. Subsequent registrations of the
    /// same interface id replace the channel (last writer wins).
    ///
    /// Uses [`DEFAULT_EVENT_BUS_CAPACITY`] as the broadcast channel
    /// capacity. For explicit control use
    /// [`Self::register_channel_with_capacity`].
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    pub fn register_channel<E>(&self, interface: PluginEventKind) -> broadcast::Sender<Arc<E>>
    where
        E: Any + Send + Sync + 'static,
    {
        self.register_channel_with_capacity::<E>(interface, DEFAULT_EVENT_BUS_CAPACITY)
    }

    /// Like [`Self::register_channel`] but with an explicit capacity.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    pub fn register_channel_with_capacity<E>(
        &self,
        interface: PluginEventKind,
        capacity: usize,
    ) -> broadcast::Sender<Arc<E>>
    where
        E: Any + Send + Sync + 'static,
    {
        let (sender, _) = broadcast::channel::<Arc<E>>(capacity);
        let entry = ChannelEntry {
            sender: Arc::new(sender.clone()),
            payload_type_id: TypeId::of::<E>(),
            payload_type_name: std::any::type_name::<E>(),
        };
        let mut guard = self.entries.write().expect("event bus lock poisoned");
        guard.insert(interface, entry);
        sender
    }

    /// Emit an event on the channel registered for `interface`.
    ///
    /// Returns the number of subscribers the event was queued for
    /// (same semantics as [`broadcast::Sender::send`]'s `Ok` path).
    ///
    /// # Errors
    ///
    /// Returns [`EventBusError::ChannelNotRegistered`] when no plugin
    /// has registered the interface yet;
    /// [`EventBusError::PayloadTypeMismatch`] when the registered
    /// channel's payload type differs from `E`. If all subscribers
    /// have been dropped, the underlying broadcast `send` error is
    /// swallowed and `Ok(0)` is returned (matching the "fire and
    /// forget" event model).
    pub fn emit<E>(&self, interface: &PluginEventKind, event: E) -> EventBusResult<usize>
    where
        E: Any + Send + Sync + 'static,
    {
        let sender = self.typed_sender::<E>(interface)?;
        Ok(sender.send(Arc::new(event)).unwrap_or(0))
    }

    /// Subscribe to events emitted on `interface`.
    ///
    /// # Errors
    ///
    /// Same error conditions as [`Self::emit`] minus the "no
    /// subscribers" case (a subscription always succeeds if the
    /// channel is registered and the payload type matches).
    pub fn subscribe<E>(
        &self,
        interface: &PluginEventKind,
    ) -> EventBusResult<broadcast::Receiver<Arc<E>>>
    where
        E: Any + Send + Sync + 'static,
    {
        let sender = self.typed_sender::<E>(interface)?;
        Ok(sender.subscribe())
    }

    /// Number of distinct interfaces currently registered.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.read().expect("event bus lock poisoned").len()
    }

    /// `true` when no interfaces have registered channels.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // clippy's `significant_drop_tightening` cannot see that the
    // `guard.get(...)` reference is what extends the guard's
    // lifetime; the scoped clone-out pattern here is intentional.
    #[allow(clippy::significant_drop_tightening)]
    fn typed_sender<E>(
        &self,
        interface: &PluginEventKind,
    ) -> EventBusResult<broadcast::Sender<Arc<E>>>
    where
        E: Any + Send + Sync + 'static,
    {
        let (sender_arc, payload_type_id, payload_type_name) = {
            let guard = self.entries.read().expect("event bus lock poisoned");
            let entry =
                guard
                    .get(interface)
                    .ok_or_else(|| EventBusError::ChannelNotRegistered {
                        interface: interface.as_str().to_string(),
                    })?;
            (
                entry.sender.clone(),
                entry.payload_type_id,
                entry.payload_type_name,
            )
        };
        if payload_type_id != TypeId::of::<E>() {
            return Err(EventBusError::PayloadTypeMismatch {
                interface: interface.as_str().to_string(),
                expected: payload_type_name,
                actual: std::any::type_name::<E>(),
            });
        }
        let downcast = sender_arc
            .downcast::<broadcast::Sender<Arc<E>>>()
            .map_err(|_| EventBusError::PayloadTypeMismatch {
                interface: interface.as_str().to_string(),
                expected: payload_type_name,
                actual: std::any::type_name::<E>(),
            })?;
        Ok((*downcast).clone())
    }
}

/// Process-wide shared event bus instance.
///
/// Plugins register channels into this singleton during `activate`;
/// any code holding a reference to it can later emit or subscribe.
#[must_use]
pub fn global_event_bus() -> Arc<EventBus> {
    use std::sync::OnceLock;
    static GLOBAL: OnceLock<Arc<EventBus>> = OnceLock::new();
    GLOBAL.get_or_init(|| Arc::new(EventBus::new())).clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_sdk::PluginEventKind;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SampleEvent {
        payload: u32,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct OtherEvent {
        value: String,
    }

    const TEST_IFACE: PluginEventKind = PluginEventKind::from_static("test.plugin/test-events");
    const OTHER_IFACE: PluginEventKind = PluginEventKind::from_static("test.plugin/other-events");

    #[tokio::test]
    async fn register_emit_subscribe_round_trip() {
        let bus = EventBus::new();
        let _sender = bus.register_channel::<SampleEvent>(TEST_IFACE);
        let mut subscriber = bus.subscribe::<SampleEvent>(&TEST_IFACE).unwrap();

        bus.emit(&TEST_IFACE, SampleEvent { payload: 42 }).unwrap();

        let received = subscriber.recv().await.expect("should receive event");
        assert_eq!(received.as_ref(), &SampleEvent { payload: 42 });
    }

    #[tokio::test]
    async fn multiple_subscribers_receive_fanout() {
        let bus = EventBus::new();
        bus.register_channel::<SampleEvent>(TEST_IFACE);

        let mut s1 = bus.subscribe::<SampleEvent>(&TEST_IFACE).unwrap();
        let mut s2 = bus.subscribe::<SampleEvent>(&TEST_IFACE).unwrap();

        let count = bus.emit(&TEST_IFACE, SampleEvent { payload: 7 }).unwrap();
        assert_eq!(count, 2, "both subscribers should be counted");

        assert_eq!(s1.recv().await.unwrap().payload, 7);
        assert_eq!(s2.recv().await.unwrap().payload, 7);
    }

    #[test]
    fn emit_on_unregistered_interface_errors() {
        let bus = EventBus::new();
        let result = bus.emit(&TEST_IFACE, SampleEvent { payload: 1 });
        assert!(matches!(
            result,
            Err(EventBusError::ChannelNotRegistered { .. })
        ));
    }

    #[test]
    fn subscribe_on_unregistered_interface_errors() {
        let bus = EventBus::new();
        let result = bus.subscribe::<SampleEvent>(&TEST_IFACE);
        assert!(matches!(
            result,
            Err(EventBusError::ChannelNotRegistered { .. })
        ));
    }

    #[test]
    fn payload_type_mismatch_is_detected() {
        let bus = EventBus::new();
        bus.register_channel::<SampleEvent>(TEST_IFACE);
        let result = bus.subscribe::<OtherEvent>(&TEST_IFACE);
        assert!(matches!(
            result,
            Err(EventBusError::PayloadTypeMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn emit_with_no_subscribers_returns_zero() {
        let bus = EventBus::new();
        bus.register_channel::<SampleEvent>(TEST_IFACE);
        let count = bus.emit(&TEST_IFACE, SampleEvent { payload: 0 }).unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn independent_interfaces_do_not_interfere() {
        let bus = EventBus::new();
        bus.register_channel::<SampleEvent>(TEST_IFACE);
        bus.register_channel::<OtherEvent>(OTHER_IFACE);

        let mut sub = bus.subscribe::<OtherEvent>(&OTHER_IFACE).unwrap();
        bus.emit(&TEST_IFACE, SampleEvent { payload: 1 }).unwrap();
        bus.emit(
            &OTHER_IFACE,
            OtherEvent {
                value: "hello".to_string(),
            },
        )
        .unwrap();

        let received = sub.recv().await.unwrap();
        assert_eq!(received.value, "hello");
    }

    #[tokio::test]
    async fn global_bus_returns_same_instance() {
        let a = global_event_bus();
        let b = global_event_bus();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
