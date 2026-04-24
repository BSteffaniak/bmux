//! Host-side typed event bus keyed by `PluginEventKind`.
//!
//! Two delivery modes coexist under the same kind namespace:
//!
//! - **Broadcast channels** (classic transient events). Plugins call
//!   [`EventBus::register_channel::<E>`] to own one; subscribers call
//!   [`EventBus::subscribe::<E>`]. Payloads fan out to every live
//!   subscriber exactly once. Late subscribers miss prior emissions.
//!   Suitable for: bell, recording-started, per-tick animation ticks.
//! - **State channels** (reactive state observation). Plugins call
//!   [`EventBus::register_state_channel::<T>`] with an initial value;
//!   subscribers call [`EventBus::subscribe_state::<T>`] and receive
//!   the current value synchronously plus a [`watch::Receiver`] for
//!   live updates. New subscribers always observe the latest value —
//!   no race windows. Suitable for: focused pane, zoom status,
//!   session list.
//!
//! Both modes share the same [`PluginEventKind`] namespace. Publishing
//! or subscribing with the wrong API surfaces a
//! [`EventBusError::ChannelDeliveryMismatch`] error.
//!
//! The BPDL schema declares delivery mode at the type level: a plain
//! `events T;` generates broadcast bindings; `@state events T;`
//! generates state-channel bindings (`STATE_KIND` + `StatePayload`).
//!
//! # Lifecycle
//!
//! 1. Plugin's `activate` calls either
//!    [`EventBus::register_channel::<E>`] (broadcast) or
//!    [`EventBus::register_state_channel::<T>`] (state) for each
//!    stream it owns, keyed by the BPDL-generated constant.
//! 2. Publishers call [`EventBus::emit::<E>`] (broadcast) or
//!    [`EventBus::publish_state::<T>`] (state); state publishes
//!    replace the retained value atomically.
//! 3. Subscribers call the matching `subscribe` / `subscribe_state`.

use bmux_plugin_sdk::PluginEventKind;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::{broadcast, watch};

/// Default capacity for newly-registered broadcast channels.
///
/// Plugins can override via [`EventBus::register_channel_with_capacity`].
pub const DEFAULT_EVENT_BUS_CAPACITY: usize = 1024;

/// Delivery semantics for a registered channel.
///
/// Reported by [`EventBusError::ChannelDeliveryMismatch`] so callers
/// can identify the kind of mismatch when they call the wrong API for
/// a registered kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    /// Broadcast — transient events fanned out to live subscribers.
    Broadcast,
    /// State — last-published value retained and replayed to new
    /// subscribers before they see any live updates.
    State,
}

impl std::fmt::Display for DeliveryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Broadcast => f.write_str("broadcast"),
            Self::State => f.write_str("state"),
        }
    }
}

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
    /// The registered channel uses a different delivery mode than the
    /// caller's API implies (e.g. calling [`EventBus::emit`] on a
    /// state channel, or [`EventBus::subscribe_state`] on a broadcast
    /// channel).
    ChannelDeliveryMismatch {
        /// The interface id involved.
        interface: String,
        /// The delivery mode the caller's API expects.
        expected: DeliveryMode,
        /// The delivery mode the registered channel actually uses.
        actual: DeliveryMode,
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
            Self::ChannelDeliveryMismatch {
                interface,
                expected,
                actual,
            } => write!(
                f,
                "event channel for `{interface}` is a {actual} channel; \
                 caller's API is {expected}"
            ),
        }
    }
}

impl std::error::Error for EventBusError {}

/// Result alias for event bus operations.
pub type EventBusResult<T> = std::result::Result<T, EventBusError>;

/// The underlying transport for a registered channel. Internal — the
/// public surface keeps the two modes separate via explicit
/// `register_channel`/`register_state_channel` + their matching
/// `emit`/`publish_state`/`subscribe`/`subscribe_state` methods.
enum ChannelKind {
    /// `tokio::sync::broadcast::Sender<Arc<T>>` erased as `Arc<dyn Any>`.
    Broadcast(Arc<dyn Any + Send + Sync>),
    /// `watch::Sender<Arc<T>>` erased as `Arc<dyn Any>`. Watch state
    /// channels retain the last published value and replay it to new
    /// subscribers.
    State(Arc<dyn Any + Send + Sync>),
}

/// Internal handle stored behind an `Arc<dyn Any>`, giving the bus a
/// uniform type to key on even though each channel's payload type
/// differs.
struct ChannelEntry {
    kind: ChannelKind,
    payload_type_id: TypeId,
    payload_type_name: &'static str,
    /// Optional bytes-to-publish trampoline. Registered channels that
    /// want to receive wire-forwarded payloads (e.g. via the
    /// `Request::EmitOnPluginBus` cross-process relay) provide a
    /// decoder at registration time. Channels without a decoder can
    /// still be published to in-process via the typed
    /// [`EventBus::publish_state`] / [`EventBus::emit`] APIs; they
    /// simply can't accept wire-encoded payloads.
    decoder: Option<BytesDecoder>,
}

/// Error surface for `emit_from_bytes` decoder invocation.
#[derive(Debug, thiserror::Error)]
pub enum EventBusBytesError {
    #[error("failed to decode wire payload: {0}")]
    Decode(String),
    #[error(transparent)]
    Bus(#[from] EventBusError),
}

/// Type alias for the bytes-to-publish trampoline stored on a
/// registered channel. Given raw wire bytes, the decoder
/// deserialises them into the channel's typed payload and invokes
/// `publish_state` on the owning event bus.
type BytesDecoder = Arc<dyn Fn(&[u8]) -> Result<(), EventBusBytesError> + Send + Sync + 'static>;

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
            kind: ChannelKind::Broadcast(Arc::new(sender.clone())),
            payload_type_id: TypeId::of::<E>(),
            payload_type_name: std::any::type_name::<E>(),
            decoder: None,
        };
        let mut guard = self.entries.write().expect("event bus lock poisoned");
        guard.insert(interface, entry);
        sender
    }

    /// Register a state channel for `T` keyed by `interface`, seeded
    /// with `initial`.
    ///
    /// Unlike broadcast channels, state channels retain the last
    /// published value and replay it synchronously to any subscriber
    /// via [`Self::subscribe_state`]. Subsequent registrations of the
    /// same interface id replace the channel and reset the retained
    /// value to the new `initial`.
    ///
    /// Returns the underlying [`watch::Sender`] so the registering
    /// plugin can publish directly without re-looking up the entry.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    pub fn register_state_channel<T>(
        &self,
        interface: PluginEventKind,
        initial: T,
    ) -> watch::Sender<Arc<T>>
    where
        T: Any + Send + Sync + 'static,
    {
        let (sender, _) = watch::channel::<Arc<T>>(Arc::new(initial));
        let entry = ChannelEntry {
            kind: ChannelKind::State(Arc::new(sender.clone())),
            payload_type_id: TypeId::of::<T>(),
            payload_type_name: std::any::type_name::<T>(),
            decoder: None,
        };
        let mut guard = self.entries.write().expect("event bus lock poisoned");
        guard.insert(interface, entry);
        sender
    }

    /// Register a state channel plus a wire-bytes decoder so callers
    /// of [`Self::emit_from_bytes`] can publish on this channel
    /// without knowing its concrete payload type at compile time.
    ///
    /// Use this instead of [`Self::register_state_channel`] when the
    /// channel needs to accept wire-encoded payloads (e.g. via the
    /// cross-process `Request::EmitOnPluginBus` relay). The decoder
    /// takes the JSON-encoded bytes, deserialises them into `T`, and
    /// invokes `publish_state` on the same bus.
    ///
    /// Returns the typed sender so the registering plugin can publish
    /// directly without re-looking up the entry.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    #[allow(clippy::needless_pass_by_value)] // Consumed twice via `.clone()` into both the registry and the captured closure.
    pub fn register_state_channel_with_decoder<T>(
        self: &Arc<Self>,
        interface: PluginEventKind,
        initial: T,
    ) -> watch::Sender<Arc<T>>
    where
        T: Any + Send + Sync + 'static + serde::de::DeserializeOwned,
    {
        let sender = self.register_state_channel::<T>(interface.clone(), initial);
        let bus = Arc::downgrade(self);
        let decoder_kind = interface.clone();
        let decoder: BytesDecoder =
            Arc::new(move |bytes: &[u8]| -> Result<(), EventBusBytesError> {
                let Some(bus) = bus.upgrade() else {
                    return Err(EventBusBytesError::Decode("event bus dropped".to_string()));
                };
                let value: T = serde_json::from_slice(bytes)
                    .map_err(|err| EventBusBytesError::Decode(err.to_string()))?;
                bus.publish_state::<T>(&decoder_kind, value)?;
                Ok(())
            });
        if let Ok(mut guard) = self.entries.write()
            && let Some(entry) = guard.get_mut(&interface)
        {
            entry.decoder = Some(decoder);
        }
        sender
    }

    /// Publish a wire-encoded payload on the channel registered for
    /// `interface`. The channel must have been registered with a
    /// decoder via [`Self::register_state_channel_with_decoder`];
    /// otherwise the payload is silently dropped (returns
    /// `Ok(false)`) so early wire events before a subscribing plugin
    /// activates don't fail loudly.
    ///
    /// Returns `Ok(true)` when a decoder ran and published, `Ok(false)`
    /// when no channel or no decoder is registered.
    ///
    /// # Errors
    ///
    /// Returns [`EventBusBytesError::Decode`] when the decoder fails
    /// to parse the payload or when the underlying
    /// [`Self::publish_state`] rejects the type.
    pub fn emit_from_bytes(
        &self,
        interface: &PluginEventKind,
        payload: &[u8],
    ) -> Result<bool, EventBusBytesError> {
        let decoder = self
            .entries
            .read()
            .map_err(|_| EventBusBytesError::Decode("event bus lock poisoned".to_string()))?
            .get(interface)
            .and_then(|entry| entry.decoder.as_ref().map(Arc::clone));
        let Some(decoder) = decoder else {
            return Ok(false);
        };
        decoder(payload)?;
        Ok(true)
    }

    /// Emit an event on the broadcast channel registered for
    /// `interface`.
    ///
    /// Returns the number of subscribers the event was queued for
    /// (same semantics as [`broadcast::Sender::send`]'s `Ok` path).
    ///
    /// # Errors
    ///
    /// Returns [`EventBusError::ChannelNotRegistered`] when no plugin
    /// has registered the interface yet;
    /// [`EventBusError::PayloadTypeMismatch`] when the registered
    /// channel's payload type differs from `E`;
    /// [`EventBusError::ChannelDeliveryMismatch`] when the registered
    /// channel is a state channel (caller should use
    /// [`Self::publish_state`] instead). If all subscribers have been
    /// dropped, the underlying broadcast `send` error is swallowed
    /// and `Ok(0)` is returned (matching the "fire and forget" event
    /// model).
    pub fn emit<E>(&self, interface: &PluginEventKind, event: E) -> EventBusResult<usize>
    where
        E: Any + Send + Sync + 'static,
    {
        let sender = self.broadcast_sender::<E>(interface)?;
        Ok(sender.send(Arc::new(event)).unwrap_or(0))
    }

    /// Publish a new value on the state channel registered for
    /// `interface`. Replaces the retained value atomically; all live
    /// subscribers' [`watch::Receiver::changed`] futures wake.
    ///
    /// # Errors
    ///
    /// Same error conditions as [`Self::emit`], except the delivery
    /// mismatch surfaces when the registered channel is a broadcast
    /// channel (caller should use [`Self::emit`] instead). Publish
    /// succeeds even when no subscribers are live — the retained
    /// value is still updated for future subscribers.
    pub fn publish_state<T>(&self, interface: &PluginEventKind, value: T) -> EventBusResult<()>
    where
        T: Any + Send + Sync + 'static,
    {
        let sender = self.state_sender::<T>(interface)?;
        // `send_replace` always updates the retained value, even when
        // no receivers are live. Using `send` would return an error
        // in that case and leave late subscribers with stale data.
        sender.send_replace(Arc::new(value));
        Ok(())
    }

    /// Subscribe to events emitted on the broadcast channel for
    /// `interface`.
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
        let sender = self.broadcast_sender::<E>(interface)?;
        Ok(sender.subscribe())
    }

    /// Subscribe to the state channel for `interface`, returning the
    /// current retained value plus a [`watch::Receiver`] for live
    /// updates.
    ///
    /// The initial value reflects whatever was most recently passed
    /// to [`Self::register_state_channel`] or
    /// [`Self::publish_state`], whichever was last. The returned
    /// receiver fires on every subsequent `publish_state` call.
    ///
    /// # Errors
    ///
    /// Same as [`Self::subscribe`] but surfaces delivery mismatch
    /// when the registered channel is a broadcast channel.
    pub fn subscribe_state<T>(
        &self,
        interface: &PluginEventKind,
    ) -> EventBusResult<(Arc<T>, watch::Receiver<Arc<T>>)>
    where
        T: Any + Send + Sync + 'static,
    {
        let sender = self.state_sender::<T>(interface)?;
        let rx = sender.subscribe();
        let current = rx.borrow().clone();
        Ok((current, rx))
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
    fn broadcast_sender<E>(
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
            match &entry.kind {
                ChannelKind::Broadcast(sender) => (
                    sender.clone(),
                    entry.payload_type_id,
                    entry.payload_type_name,
                ),
                ChannelKind::State(_) => {
                    return Err(EventBusError::ChannelDeliveryMismatch {
                        interface: interface.as_str().to_string(),
                        expected: DeliveryMode::Broadcast,
                        actual: DeliveryMode::State,
                    });
                }
            }
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

    #[allow(clippy::significant_drop_tightening)]
    fn state_sender<T>(&self, interface: &PluginEventKind) -> EventBusResult<watch::Sender<Arc<T>>>
    where
        T: Any + Send + Sync + 'static,
    {
        let (sender_arc, payload_type_id, payload_type_name) = {
            let guard = self.entries.read().expect("event bus lock poisoned");
            let entry =
                guard
                    .get(interface)
                    .ok_or_else(|| EventBusError::ChannelNotRegistered {
                        interface: interface.as_str().to_string(),
                    })?;
            match &entry.kind {
                ChannelKind::State(sender) => (
                    sender.clone(),
                    entry.payload_type_id,
                    entry.payload_type_name,
                ),
                ChannelKind::Broadcast(_) => {
                    return Err(EventBusError::ChannelDeliveryMismatch {
                        interface: interface.as_str().to_string(),
                        expected: DeliveryMode::State,
                        actual: DeliveryMode::Broadcast,
                    });
                }
            }
        };
        if payload_type_id != TypeId::of::<T>() {
            return Err(EventBusError::PayloadTypeMismatch {
                interface: interface.as_str().to_string(),
                expected: payload_type_name,
                actual: std::any::type_name::<T>(),
            });
        }
        let downcast = sender_arc
            .downcast::<watch::Sender<Arc<T>>>()
            .map_err(|_| EventBusError::PayloadTypeMismatch {
                interface: interface.as_str().to_string(),
                expected: payload_type_name,
                actual: std::any::type_name::<T>(),
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct FocusSnapshot {
        focused: Option<u64>,
        revision: u64,
    }

    const TEST_IFACE: PluginEventKind = PluginEventKind::from_static("test.plugin/test-events");
    const OTHER_IFACE: PluginEventKind = PluginEventKind::from_static("test.plugin/other-events");
    const STATE_IFACE: PluginEventKind = PluginEventKind::from_static("test.plugin/focus-state");

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

    // ── State-channel primitive tests ───────────────────────────────

    #[tokio::test]
    async fn state_channel_subscribe_returns_initial_value_before_any_publish() {
        let bus = EventBus::new();
        bus.register_state_channel::<FocusSnapshot>(
            STATE_IFACE,
            FocusSnapshot {
                focused: None,
                revision: 0,
            },
        );
        let (initial, _rx) = bus.subscribe_state::<FocusSnapshot>(&STATE_IFACE).unwrap();
        assert_eq!(
            initial.as_ref(),
            &FocusSnapshot {
                focused: None,
                revision: 0,
            },
        );
    }

    #[tokio::test]
    async fn state_channel_replays_latest_value_to_late_subscribers() {
        let bus = EventBus::new();
        bus.register_state_channel::<FocusSnapshot>(
            STATE_IFACE,
            FocusSnapshot {
                focused: None,
                revision: 0,
            },
        );
        // Publish before anyone subscribes. Classic broadcast would
        // drop these; state channel retains them.
        bus.publish_state(
            &STATE_IFACE,
            FocusSnapshot {
                focused: Some(7),
                revision: 1,
            },
        )
        .unwrap();
        bus.publish_state(
            &STATE_IFACE,
            FocusSnapshot {
                focused: Some(8),
                revision: 2,
            },
        )
        .unwrap();

        // A subscriber arriving now should see the most recent
        // snapshot, not the initial value and not an intermediate.
        let (initial, _rx) = bus.subscribe_state::<FocusSnapshot>(&STATE_IFACE).unwrap();
        assert_eq!(
            initial.as_ref(),
            &FocusSnapshot {
                focused: Some(8),
                revision: 2,
            },
        );
    }

    #[tokio::test]
    async fn state_channel_pushes_live_updates_to_existing_subscribers() {
        let bus = EventBus::new();
        bus.register_state_channel::<FocusSnapshot>(
            STATE_IFACE,
            FocusSnapshot {
                focused: None,
                revision: 0,
            },
        );
        let (_initial, mut rx) = bus.subscribe_state::<FocusSnapshot>(&STATE_IFACE).unwrap();
        bus.publish_state(
            &STATE_IFACE,
            FocusSnapshot {
                focused: Some(1),
                revision: 1,
            },
        )
        .unwrap();
        rx.changed().await.expect("watch should fire");
        let snapshot = rx.borrow().clone();
        assert_eq!(
            snapshot.as_ref(),
            &FocusSnapshot {
                focused: Some(1),
                revision: 1,
            },
        );
    }

    #[test]
    fn emit_on_state_channel_errors_with_delivery_mismatch() {
        let bus = EventBus::new();
        bus.register_state_channel::<FocusSnapshot>(
            STATE_IFACE,
            FocusSnapshot {
                focused: None,
                revision: 0,
            },
        );
        let err = bus
            .emit(
                &STATE_IFACE,
                FocusSnapshot {
                    focused: Some(1),
                    revision: 1,
                },
            )
            .expect_err("emit on state channel must fail");
        match err {
            EventBusError::ChannelDeliveryMismatch {
                expected, actual, ..
            } => {
                assert_eq!(expected, DeliveryMode::Broadcast);
                assert_eq!(actual, DeliveryMode::State);
            }
            other => panic!("expected delivery mismatch, got {other:?}"),
        }
    }

    #[test]
    fn publish_state_on_broadcast_channel_errors_with_delivery_mismatch() {
        let bus = EventBus::new();
        bus.register_channel::<SampleEvent>(TEST_IFACE);
        let err = bus
            .publish_state(&TEST_IFACE, SampleEvent { payload: 1 })
            .expect_err("publish_state on broadcast channel must fail");
        match err {
            EventBusError::ChannelDeliveryMismatch {
                expected, actual, ..
            } => {
                assert_eq!(expected, DeliveryMode::State);
                assert_eq!(actual, DeliveryMode::Broadcast);
            }
            other => panic!("expected delivery mismatch, got {other:?}"),
        }
    }

    #[test]
    fn subscribe_state_on_broadcast_channel_errors_with_delivery_mismatch() {
        let bus = EventBus::new();
        bus.register_channel::<SampleEvent>(TEST_IFACE);
        let err = bus
            .subscribe_state::<SampleEvent>(&TEST_IFACE)
            .expect_err("subscribe_state on broadcast channel must fail");
        assert!(matches!(err, EventBusError::ChannelDeliveryMismatch { .. }));
    }

    #[test]
    fn subscribe_on_state_channel_errors_with_delivery_mismatch() {
        let bus = EventBus::new();
        bus.register_state_channel::<FocusSnapshot>(
            STATE_IFACE,
            FocusSnapshot {
                focused: None,
                revision: 0,
            },
        );
        let err = bus
            .subscribe::<FocusSnapshot>(&STATE_IFACE)
            .expect_err("subscribe on state channel must fail");
        assert!(matches!(err, EventBusError::ChannelDeliveryMismatch { .. }));
    }

    #[test]
    fn state_channel_payload_type_mismatch_is_detected() {
        let bus = EventBus::new();
        bus.register_state_channel::<FocusSnapshot>(
            STATE_IFACE,
            FocusSnapshot {
                focused: None,
                revision: 0,
            },
        );
        let err = bus
            .subscribe_state::<SampleEvent>(&STATE_IFACE)
            .expect_err("wrong payload type should fail");
        assert!(matches!(err, EventBusError::PayloadTypeMismatch { .. }));
    }
}
