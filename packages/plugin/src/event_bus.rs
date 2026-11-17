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
//!
//! # Ownership
//!
//! There is no process-wide bus. The host program constructs one root bus
//! with [`EventBus::new`] and hands each plugin activation a scoped view
//! from [`EventBus::scoped`] through the activation's capabilities (see
//! [`activation_binder`]). A scoped view shares the root's channels but owns
//! the registrations made through it: they are removed when the view is
//! dropped or its activation scope closes, and every operation on a view
//! whose scope has closed fails with [`EventBusError::Closed`]. Two
//! activations (including two instances of one plugin) therefore never share
//! ownership, and a dynamically loaded library cannot reach a bus it was not
//! handed.
//!
//! # Dynamic plugins
//!
//! A dynamic library cannot receive the in-process view: its copy of every
//! type is distinct. The host instead serves the same scoped view through a
//! serialized port on the plug-rt table (see [`remote`]), and inside the
//! library [`take_activation_event_bus`] returns a remote `EventBus` over it.
//! The serialized surface works identically in both modes and is what
//! portable plugin code uses: [`EventBus::register_json_channel`],
//! [`EventBus::emit_json`], [`EventBus::emit_from_bytes`],
//! [`EventBus::subscribe_json`] and [`EventBus::subscribe_state_json`].
//! Typed `emit`/`publish_state`/`subscribe`/`subscribe_state` need an
//! in-process bus and report [`EventBusError::RemoteSerializedOnly`] on a
//! remote one; typed registrations on a remote bus stay plugin-local.

pub mod remote;

use bmux_plugin_runtime::{ActivationScope, AdmissionGuard};
use bmux_plugin_sdk::PluginEventKind;
use bmux_plugin_sdk::definition::ActivationCapabilities;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
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

/// Type-erased JSON projection of a typed plugin event.
///
/// Typed plugin channels remain the source of truth; this is an optional,
/// lazily-produced projection for generic consumers such as decoration scripts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPluginEvent {
    pub interface: PluginEventKind,
    pub delivery: DeliveryMode,
    pub payload: JsonValue,
}

/// Controls whether a typed channel exposes a dynamic JSON projection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JsonProjectionPolicy {
    /// No JSON projection is registered. Typed subscribers remain fully
    /// functional and no JSON serialization is performed.
    #[default]
    Disabled,
    /// JSON is serialized only when a JSON subscriber is active. For state
    /// channels, the current retained state is serialized on first JSON
    /// subscription.
    Lazy,
}

/// Registration options for event-bus channels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventBusChannelOptions {
    pub json_projection: JsonProjectionPolicy,
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
    /// The channel exists but was registered without a type-erased JSON projection.
    JsonProjectionUnavailable {
        /// The interface id involved.
        interface: String,
    },
    /// The activation scope that owns this bus view has closed; nothing is
    /// admitted through it any more.
    Closed,
    /// A dynamic plugin's bus is remote: typed in-process values cannot
    /// cross the library boundary. Use the serialized JSON surface.
    RemoteSerializedOnly {
        /// The interface id involved.
        interface: String,
    },
    /// The host refused a serialized operation on a remote bus.
    Remote(String),
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
            Self::JsonProjectionUnavailable { interface } => write!(
                f,
                "event channel for `{interface}` was registered without a JSON projection"
            ),
            Self::Closed => f.write_str("event bus view closed with its activation scope"),
            Self::RemoteSerializedOnly { interface } => write!(
                f,
                "event channel `{interface}` is reached through a dynamic plugin's remote bus; \
                 use the serialized JSON API"
            ),
            Self::Remote(reason) => write!(f, "remote event bus: {reason}"),
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

type JsonEventEncoder =
    Arc<dyn Fn(&(dyn Any + Send + Sync)) -> Option<JsonValue> + Send + Sync + 'static>;
type JsonStateEncoder = Arc<dyn Fn(&ChannelKind) -> Option<JsonValue> + Send + Sync + 'static>;

struct JsonProjection {
    broadcast: broadcast::Sender<Arc<JsonPluginEvent>>,
    state: Option<watch::Sender<Arc<JsonPluginEvent>>>,
    event_encoder: Option<JsonEventEncoder>,
    state_encoder: Option<JsonStateEncoder>,
}

/// Internal handle stored behind an `Arc<dyn Any>`, giving the bus a
/// uniform type to key on even though each channel's payload type
/// differs.
struct ChannelEntry {
    /// Bus-unique registration identity, so an owner only ever removes the
    /// registration it made (never a newer one under the same kind).
    registration: u64,
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
    json: Option<JsonProjection>,
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

fn event_json_encoder<T>() -> JsonEventEncoder
where
    T: Any + Send + Sync + Serialize + 'static,
{
    Arc::new(|event| serde_json::to_value(event.downcast_ref::<T>()?).ok())
}

fn state_json_encoder<T>() -> JsonStateEncoder
where
    T: Any + Send + Sync + Serialize + 'static,
{
    Arc::new(|kind| {
        let ChannelKind::State(sender) = kind else {
            return None;
        };
        let sender = sender.clone().downcast::<watch::Sender<Arc<T>>>().ok()?;
        serde_json::to_value(sender.borrow().as_ref()).ok()
    })
}

/// Channels shared by a root bus and every view scoped from it.
#[derive(Default)]
struct BusCore {
    entries: RwLock<HashMap<PluginEventKind, ChannelEntry>>,
    registrations: watch::Sender<()>,
    next_registration: AtomicU64,
}

/// The activation that owns a scoped view, and what it registered.
struct ScopeOwner {
    scope: ActivationScope,
    owned: Mutex<Vec<(PluginEventKind, u64)>>,
}

/// Typed event bus: either a host-owned root or an activation-scoped view.
pub struct EventBus {
    core: Arc<BusCore>,
    owner: Option<ScopeOwner>,
    /// Set inside a dynamic plugin: the host's serialized event port.
    remote: Option<bmux_plugin_runtime::rt::events::RemoteEvents>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for EventBus {
    fn drop(&mut self) {
        self.release_owned();
    }
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.core.entries.read().map_or(0, |g| g.len());
        f.debug_struct("EventBus")
            .field("channels", &count)
            .field("scoped", &self.owner.is_some())
            .finish_non_exhaustive()
    }
}

impl EventBus {
    /// Construct an empty root bus. The host program owns it and hands
    /// plugins scoped views; nothing else can reach it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            core: Arc::default(),
            owner: None,
            remote: None,
        }
    }

    /// A dynamic plugin's view of its activation's host bus, over the
    /// serialized event port.
    #[must_use]
    pub fn remote(events: bmux_plugin_runtime::rt::events::RemoteEvents) -> Arc<Self> {
        Arc::new(Self {
            core: Arc::default(),
            owner: None,
            remote: Some(events),
        })
    }

    /// Whether this bus reaches the host through the serialized port.
    #[must_use]
    pub const fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    fn typed_on_remote(&self, interface: &PluginEventKind) -> EventBusResult<()> {
        if self.remote.is_some() {
            return Err(EventBusError::RemoteSerializedOnly {
                interface: interface.as_str().to_string(),
            });
        }
        Ok(())
    }

    /// Register a broadcast channel of JSON values, owned like any other
    /// registration through this view. It accepts serialized input
    /// ([`Self::emit_json`], [`Self::emit_from_bytes`]) and serves typed
    /// (`serde_json::Value`) and JSON subscribers. Works on a remote bus.
    ///
    /// # Errors
    /// The view's scope closed, or the host refused a remote registration.
    pub fn register_json_channel(
        self: &Arc<Self>,
        interface: PluginEventKind,
    ) -> EventBusResult<()> {
        if let Some(remote) = &self.remote {
            return remote
                .register(interface.as_str())
                .map_err(remote::port_error);
        }
        self.admit()?;
        let kind = interface.clone();
        self.register_channel_with_json_projection::<JsonValue>(interface);
        let bus = Arc::downgrade(self);
        let emit_kind = kind.clone();
        let decoder: BytesDecoder = Arc::new(move |bytes: &[u8]| {
            let bus = bus
                .upgrade()
                .ok_or_else(|| EventBusBytesError::Decode("event bus dropped".to_string()))?;
            let value: JsonValue = serde_json::from_slice(bytes)
                .map_err(|error| EventBusBytesError::Decode(error.to_string()))?;
            bus.emit::<JsonValue>(&emit_kind, value)?;
            Ok(())
        });
        self.set_bytes_decoder(&kind, decoder);
        Ok(())
    }

    /// Register a retained state channel of JSON values, owned like any
    /// other registration through this view. Its value is JSON `null` until
    /// the first publication; it accepts serialized input
    /// ([`Self::emit_json`], [`Self::emit_from_bytes`]) and serves typed
    /// (`serde_json::Value`) and JSON state subscribers. Works on a remote
    /// bus.
    ///
    /// # Errors
    /// The view's scope closed, or the host refused a remote registration.
    pub fn register_json_state_channel(
        self: &Arc<Self>,
        interface: PluginEventKind,
    ) -> EventBusResult<()> {
        if let Some(remote) = &self.remote {
            return remote
                .register_state(interface.as_str())
                .map_err(remote::port_error);
        }
        self.admit()?;
        let kind = interface.clone();
        self.register_state_channel_with_json_projection::<JsonValue>(interface, JsonValue::Null);
        self.install_state_bytes_decoder::<JsonValue, _>(&kind, |bytes| {
            serde_json::from_slice(bytes)
                .map_err(|error| EventBusBytesError::Decode(error.to_string()))
        });
        Ok(())
    }

    /// Publish a JSON payload on a channel that accepts serialized input
    /// (a JSON channel, or a typed channel registered with a decoder).
    /// Works on a remote bus.
    ///
    /// # Errors
    /// As [`Self::emit_from_bytes`].
    pub fn emit_json(
        &self,
        interface: &PluginEventKind,
        payload: &JsonValue,
    ) -> Result<bool, EventBusBytesError> {
        let bytes = serde_json::to_vec(payload)
            .map_err(|error| EventBusBytesError::Decode(error.to_string()))?;
        self.emit_from_bytes(interface, &bytes)
    }

    /// A view of this bus owned by one plugin activation.
    ///
    /// The view shares every channel with `self`. Channels registered
    /// through it belong to the activation: they are removed when the view
    /// is dropped or `scope` closes. Once `scope` closes, every emit,
    /// publish and subscribe through the view fails with
    /// [`EventBusError::Closed`] and registrations are not admitted.
    #[must_use]
    pub fn scoped(self: &Arc<Self>, scope: ActivationScope) -> Arc<Self> {
        Arc::new(Self {
            core: Arc::clone(&self.core),
            owner: Some(ScopeOwner {
                scope,
                owned: Mutex::new(Vec::new()),
            }),
            remote: None,
        })
    }

    /// Whether `self` and `other` deliver on the same channels.
    #[must_use]
    pub fn shares_channels_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.core, &other.core)
    }

    /// Admit one operation through a scoped view. A closed scope releases
    /// the view's registrations and rejects the operation.
    fn admit(&self) -> EventBusResult<Option<AdmissionGuard>> {
        let Some(owner) = &self.owner else {
            return Ok(None);
        };
        owner.scope.try_admit().map_or_else(
            |_| {
                self.release_owned();
                Err(EventBusError::Closed)
            },
            |guard| Ok(Some(guard)),
        )
    }

    /// Publish `entry` under `interface`, recording ownership for a scoped
    /// view. Returns `false` (and publishes nothing) when the view's scope
    /// has closed.
    fn insert_entry(&self, interface: PluginEventKind, mut entry: ChannelEntry) -> bool {
        let Ok(admission) = self.admit() else {
            return false;
        };
        let registration = self.core.next_registration.fetch_add(1, Ordering::Relaxed);
        entry.registration = registration;
        if let Some(owner) = &self.owner
            && let Ok(mut owned) = owner.owned.lock()
        {
            owned.push((interface.clone(), registration));
        }
        let mut guard = self.core.entries.write().expect("event bus lock poisoned");
        guard.insert(interface, entry);
        drop(guard);
        drop(admission);
        self.core.registrations.send_replace(());
        true
    }

    /// Remove every channel this view registered that has not since been
    /// replaced by another registration.
    fn release_owned(&self) {
        let Some(owner) = &self.owner else {
            return;
        };
        let released = owner
            .owned
            .lock()
            .map(|mut registrations| std::mem::take(&mut *registrations))
            .unwrap_or_default();
        if released.is_empty() {
            return;
        }
        if let Ok(mut entries) = self.core.entries.write() {
            for (interface, registration) in released {
                if entries
                    .get(&interface)
                    .is_some_and(|entry| entry.registration == registration)
                {
                    entries.remove(&interface);
                }
            }
        }
        self.core.registrations.send_replace(());
    }

    /// Await a broadcast provider's registration without polling. Subscribing
    /// to registration changes before checking the catalog prevents lost wakes.
    ///
    /// # Errors
    /// Returns incompatible channel type or delivery errors immediately.
    pub async fn subscribe_when_registered<E>(
        &self,
        interface: &PluginEventKind,
    ) -> EventBusResult<broadcast::Receiver<Arc<E>>>
    where
        E: Any + Send + Sync + 'static,
    {
        let mut registrations = self.core.registrations.subscribe();
        loop {
            match self.subscribe::<E>(interface) {
                Err(EventBusError::ChannelNotRegistered { .. }) => {
                    let _ = registrations.changed().await;
                }
                result => return result,
            }
        }
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

    /// Register a broadcast channel with lazy JSON projection enabled.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    pub fn register_channel_with_json_projection<E>(
        &self,
        interface: PluginEventKind,
    ) -> broadcast::Sender<Arc<E>>
    where
        E: Any + Send + Sync + Serialize + 'static,
    {
        self.register_channel_with_capacity_and_options::<E>(
            interface,
            DEFAULT_EVENT_BUS_CAPACITY,
            EventBusChannelOptions {
                json_projection: JsonProjectionPolicy::Lazy,
            },
        )
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
        self.register_channel_with_capacity_and_json_encoder::<E>(interface, capacity, None)
    }

    /// Register a broadcast channel with explicit options and capacity.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    pub fn register_channel_with_capacity_and_options<E>(
        &self,
        interface: PluginEventKind,
        capacity: usize,
        options: EventBusChannelOptions,
    ) -> broadcast::Sender<Arc<E>>
    where
        E: Any + Send + Sync + Serialize + 'static,
    {
        let event_encoder = (options.json_projection == JsonProjectionPolicy::Lazy)
            .then(|| event_json_encoder::<E>());
        self.register_channel_with_capacity_and_json_encoder::<E>(
            interface,
            capacity,
            event_encoder,
        )
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
            registration: 0,
            kind: ChannelKind::State(Arc::new(sender.clone())),
            payload_type_id: TypeId::of::<T>(),
            payload_type_name: std::any::type_name::<T>(),
            decoder: None,
            json: None,
        };
        self.insert_entry(interface, entry);
        sender
    }

    /// Register a state channel with lazy JSON projection enabled.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    pub fn register_state_channel_with_json_projection<T>(
        &self,
        interface: PluginEventKind,
        initial: T,
    ) -> watch::Sender<Arc<T>>
    where
        T: Any + Send + Sync + Serialize + 'static,
    {
        self.register_state_channel_with_options(
            interface,
            initial,
            EventBusChannelOptions {
                json_projection: JsonProjectionPolicy::Lazy,
            },
        )
    }

    /// Register a retained state channel with explicit options.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    pub fn register_state_channel_with_options<T>(
        &self,
        interface: PluginEventKind,
        initial: T,
        options: EventBusChannelOptions,
    ) -> watch::Sender<Arc<T>>
    where
        T: Any + Send + Sync + Serialize + 'static,
    {
        let (sender, _) = watch::channel::<Arc<T>>(Arc::new(initial));
        let json = if options.json_projection == JsonProjectionPolicy::Lazy {
            let initial_json = JsonPluginEvent {
                interface: interface.clone(),
                delivery: DeliveryMode::State,
                payload: JsonValue::Null,
            };
            let (json_state_sender, _) =
                watch::channel::<Arc<JsonPluginEvent>>(Arc::new(initial_json));
            let (json_broadcast_sender, _) =
                broadcast::channel::<Arc<JsonPluginEvent>>(DEFAULT_EVENT_BUS_CAPACITY);
            Some(JsonProjection {
                broadcast: json_broadcast_sender,
                state: Some(json_state_sender),
                event_encoder: None,
                state_encoder: Some(state_json_encoder::<T>()),
            })
        } else {
            None
        };
        let entry = ChannelEntry {
            registration: 0,
            kind: ChannelKind::State(Arc::new(sender.clone())),
            payload_type_id: TypeId::of::<T>(),
            payload_type_name: std::any::type_name::<T>(),
            decoder: None,
            json,
        };
        self.insert_entry(interface, entry);
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
        self.register_state_channel_with_bytes_decoder(interface, initial, |bytes| {
            serde_json::from_slice(bytes).map_err(|err| EventBusBytesError::Decode(err.to_string()))
        })
    }

    /// Register a retained state channel with a custom wire decoder.
    ///
    /// Use this for non-JSON binary state payloads that arrive through
    /// [`Self::emit_from_bytes`]. The decoder converts each byte payload into
    /// the retained state value published on the channel.
    #[allow(clippy::needless_pass_by_value)] // Consumed via `.clone()` for registration and decoder capture; matches state-channel registration APIs.
    pub fn register_state_channel_with_bytes_decoder<T, F>(
        self: &Arc<Self>,
        interface: PluginEventKind,
        initial: T,
        decode: F,
    ) -> watch::Sender<Arc<T>>
    where
        T: Any + Send + Sync + 'static,
        F: Fn(&[u8]) -> Result<T, EventBusBytesError> + Send + Sync + 'static,
    {
        let sender = self.register_state_channel::<T>(interface.clone(), initial);
        self.install_state_bytes_decoder(&interface, decode);
        sender
    }

    fn install_state_bytes_decoder<T, F>(self: &Arc<Self>, interface: &PluginEventKind, decode: F)
    where
        T: Any + Send + Sync + 'static,
        F: Fn(&[u8]) -> Result<T, EventBusBytesError> + Send + Sync + 'static,
    {
        let bus = Arc::downgrade(self);
        let decoder_kind = interface.clone();
        let decoder: BytesDecoder =
            Arc::new(move |bytes: &[u8]| -> Result<(), EventBusBytesError> {
                let Some(bus) = bus.upgrade() else {
                    return Err(EventBusBytesError::Decode("event bus dropped".to_string()));
                };
                let value = decode(bytes)?;
                bus.publish_state::<T>(&decoder_kind, value)?;
                Ok(())
            });
        self.set_bytes_decoder(interface, decoder);
    }

    fn set_bytes_decoder(&self, interface: &PluginEventKind, decoder: BytesDecoder) {
        if let Ok(mut guard) = self.core.entries.write()
            && let Some(entry) = guard.get_mut(interface)
        {
            entry.decoder = Some(decoder);
        }
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
        if let Some(remote) = &self.remote {
            return remote::publish(remote, interface, payload);
        }
        let _admission = self.admit()?;
        let decoder = self
            .core
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
        self.typed_on_remote(interface)?;
        let _admission = self.admit()?;
        let sender = self.broadcast_sender::<E>(interface)?;
        let event = Arc::new(event);
        let json_payload = if self.json_projection_observed(interface, DeliveryMode::Broadcast) {
            self.project_json_event(interface, event.as_ref())
        } else {
            None
        };
        let count = sender.send(event).unwrap_or(0);
        if let Some(payload) = json_payload {
            self.publish_json_projection(interface, DeliveryMode::Broadcast, payload);
        }
        Ok(count)
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
        self.typed_on_remote(interface)?;
        let _admission = self.admit()?;
        let sender = self.state_sender::<T>(interface)?;
        // `send_replace` always updates the retained value, even when
        // no receivers are live. Using `send` would return an error
        // in that case and leave late subscribers with stale data.
        sender.send_replace(Arc::new(value));
        self.publish_json_projection_from_state_if_observed(interface);
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
        self.typed_on_remote(interface)?;
        let _admission = self.admit()?;
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
        self.typed_on_remote(interface)?;
        let _admission = self.admit()?;
        let sender = self.state_sender::<T>(interface)?;
        let rx = sender.subscribe();
        let current = rx.borrow().clone();
        Ok((current, rx))
    }

    /// Subscribe to a type-erased JSON projection of a broadcast channel.
    ///
    /// # Errors
    ///
    /// Returns the same registration/delivery errors as typed
    /// [`Self::subscribe`], but without payload type checks.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    #[allow(clippy::significant_drop_tightening)] // We clone the sender out while holding the registry guard, then subscribe after the guard drops.
    pub fn subscribe_json(
        &self,
        interface: &PluginEventKind,
    ) -> EventBusResult<broadcast::Receiver<Arc<JsonPluginEvent>>> {
        if let Some(remote) = &self.remote {
            return remote::subscribe_json(remote, interface);
        }
        let _admission = self.admit()?;
        let sender = {
            let guard = self.core.entries.read().expect("event bus lock poisoned");
            let entry =
                guard
                    .get(interface)
                    .ok_or_else(|| EventBusError::ChannelNotRegistered {
                        interface: interface.as_str().to_string(),
                    })?;
            if !matches!(entry.kind, ChannelKind::Broadcast(_)) {
                return Err(EventBusError::ChannelDeliveryMismatch {
                    interface: interface.as_str().to_string(),
                    expected: DeliveryMode::Broadcast,
                    actual: DeliveryMode::State,
                });
            }
            let Some(json) = entry.json.as_ref() else {
                return Err(EventBusError::JsonProjectionUnavailable {
                    interface: interface.as_str().to_string(),
                });
            };
            json.broadcast.clone()
        };
        Ok(sender.subscribe())
    }

    /// Subscribe to a type-erased JSON projection of a state channel.
    /// Returns the current retained JSON value plus live updates.
    ///
    /// # Errors
    ///
    /// Returns the same registration/delivery errors as typed
    /// [`Self::subscribe_state`], but without payload type checks.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    #[allow(clippy::significant_drop_tightening)] // We clone the sender out while holding the registry guard, then subscribe after the guard drops.
    pub fn subscribe_state_json(
        &self,
        interface: &PluginEventKind,
    ) -> EventBusResult<(Arc<JsonPluginEvent>, watch::Receiver<Arc<JsonPluginEvent>>)> {
        if let Some(remote) = &self.remote {
            return remote::subscribe_state_json(remote, interface);
        }
        let _admission = self.admit()?;
        let (sender, current) = {
            let guard = self.core.entries.read().expect("event bus lock poisoned");
            let entry =
                guard
                    .get(interface)
                    .ok_or_else(|| EventBusError::ChannelNotRegistered {
                        interface: interface.as_str().to_string(),
                    })?;
            let Some(json) = entry.json.as_ref() else {
                return Err(EventBusError::JsonProjectionUnavailable {
                    interface: interface.as_str().to_string(),
                });
            };
            let Some(sender) = json.state.as_ref() else {
                return Err(EventBusError::ChannelDeliveryMismatch {
                    interface: interface.as_str().to_string(),
                    expected: DeliveryMode::State,
                    actual: DeliveryMode::Broadcast,
                });
            };
            let Some(encoder) = json.state_encoder.as_ref() else {
                return Err(EventBusError::ChannelDeliveryMismatch {
                    interface: interface.as_str().to_string(),
                    expected: DeliveryMode::State,
                    actual: DeliveryMode::Broadcast,
                });
            };
            let payload = encoder(&entry.kind).unwrap_or(JsonValue::Null);
            let current = Arc::new(JsonPluginEvent {
                interface: interface.clone(),
                delivery: DeliveryMode::State,
                payload,
            });
            sender.send_replace(Arc::clone(&current));
            (sender.clone(), current)
        };
        let rx = sender.subscribe();
        Ok((current, rx))
    }

    /// Number of distinct interfaces currently registered.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.core
            .entries
            .read()
            .expect("event bus lock poisoned")
            .len()
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
            let guard = self.core.entries.read().expect("event bus lock poisoned");
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

    fn register_channel_with_capacity_and_json_encoder<E>(
        &self,
        interface: PluginEventKind,
        capacity: usize,
        event_encoder: Option<JsonEventEncoder>,
    ) -> broadcast::Sender<Arc<E>>
    where
        E: Any + Send + Sync + 'static,
    {
        let (sender, _) = broadcast::channel::<Arc<E>>(capacity);
        let json = event_encoder.map(|event_encoder| {
            let (json_sender, _) = broadcast::channel::<Arc<JsonPluginEvent>>(capacity);
            JsonProjection {
                broadcast: json_sender,
                state: None,
                event_encoder: Some(event_encoder),
                state_encoder: None,
            }
        });
        let entry = ChannelEntry {
            registration: 0,
            kind: ChannelKind::Broadcast(Arc::new(sender.clone())),
            payload_type_id: TypeId::of::<E>(),
            payload_type_name: std::any::type_name::<E>(),
            decoder: None,
            json,
        };
        self.insert_entry(interface, entry);
        sender
    }

    fn json_projection_observed(
        &self,
        interface: &PluginEventKind,
        delivery: DeliveryMode,
    ) -> bool {
        self.core.entries.read().is_ok_and(|guard| {
            let Some(json) = guard.get(interface).and_then(|entry| entry.json.as_ref()) else {
                return false;
            };
            match delivery {
                DeliveryMode::Broadcast => json.broadcast.receiver_count() > 0,
                DeliveryMode::State => {
                    json.broadcast.receiver_count() > 0
                        || json
                            .state
                            .as_ref()
                            .is_some_and(|sender| sender.receiver_count() > 0)
                }
            }
        })
    }

    fn project_json_event(
        &self,
        interface: &PluginEventKind,
        event: &(dyn Any + Send + Sync),
    ) -> Option<JsonValue> {
        let encoder = {
            let guard = self.core.entries.read().ok()?;
            guard.get(interface)?.json.as_ref()?.event_encoder.clone()?
        };
        encoder(event)
    }

    fn publish_json_projection_from_state_if_observed(&self, interface: &PluginEventKind) {
        let payload = {
            let Ok(guard) = self.core.entries.read() else {
                return;
            };
            let Some(entry) = guard.get(interface) else {
                return;
            };
            let Some(json) = entry.json.as_ref() else {
                return;
            };
            let observed = json.broadcast.receiver_count() > 0
                || json
                    .state
                    .as_ref()
                    .is_some_and(|sender| sender.receiver_count() > 0);
            if !observed {
                return;
            }
            let Some(encoder) = json.state_encoder.as_ref() else {
                return;
            };
            encoder(&entry.kind).unwrap_or(JsonValue::Null)
        };
        self.publish_json_projection(interface, DeliveryMode::State, payload);
    }

    fn publish_json_projection(
        &self,
        interface: &PluginEventKind,
        delivery: DeliveryMode,
        payload: JsonValue,
    ) {
        let event = Arc::new(JsonPluginEvent {
            interface: interface.clone(),
            delivery,
            payload,
        });
        let Ok(guard) = self.core.entries.read() else {
            return;
        };
        let Some(entry) = guard.get(interface) else {
            return;
        };
        let Some(json) = entry.json.as_ref() else {
            return;
        };
        let _ = json.broadcast.send(event.clone());
        if let Some(sender) = json.state.as_ref() {
            sender.send_replace(event);
        }
    }

    #[allow(clippy::significant_drop_tightening)]
    fn state_sender<T>(&self, interface: &PluginEventKind) -> EventBusResult<watch::Sender<Arc<T>>>
    where
        T: Any + Send + Sync + 'static,
    {
        let (sender_arc, payload_type_id, payload_type_name) = {
            let guard = self.core.entries.read().expect("event bus lock poisoned");
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

/// Host activation binder that offers each activation a view of `bus`
/// scoped to that activation (see [`EventBus::scoped`]).
///
/// Register it on in-process executions before they activate. The plugin
/// takes the view with [`take_activation_event_bus`].
#[must_use]
pub fn activation_binder(bus: Arc<EventBus>) -> Arc<crate::ActivationBinder> {
    Arc::new(
        move |scope: &ActivationScope, capabilities: &ActivationCapabilities| {
            capabilities
                .insert(ActivationEventBus(bus.scoped(scope.clone())))
                .map_err(|error| bmux_plugin_sdk::PluginError::ServiceProtocol {
                    details: error.message,
                })
        },
    )
}

/// Take the event bus view the host supplied for a legacy author's
/// activation, reporting at activation time when there is none.
///
/// A plugin without a bus keeps running with its events disabled; it never
/// falls back to process-wide state.
#[must_use]
pub fn activation_event_bus(
    context: &bmux_plugin_sdk::NativeLifecycleContext,
) -> Option<Arc<EventBus>> {
    match take_activation_event_bus(&context.capabilities) {
        Ok(bus) => Some(bus),
        Err(error) => {
            tracing::warn!(
                plugin_id = %context.plugin_id,
                error = %error.message,
                "activation has no host event bus; this plugin's events are disabled"
            );
            None
        }
    }
}

/// The activation-scoped bus a host offers through [`activation_binder`].
pub struct ActivationEventBus(pub Arc<EventBus>);

/// Take the event bus view the host supplied for this activation.
///
/// # Errors
/// Returns an explicit unavailable error when the host supplied no bus (for
/// example a dynamically loaded artifact, which cannot receive in-process
/// values) or it was already taken. There is no process-wide fallback.
pub fn take_activation_event_bus(
    capabilities: &ActivationCapabilities,
) -> Result<Arc<EventBus>, bmux_plugin_sdk::PluginCommandError> {
    match capabilities.take::<ActivationEventBus>() {
        Ok(ActivationEventBus(bus)) => Ok(bus),
        // Inside a dynamic plugin the host serves this activation's view
        // through the serialized port of the entered instance scope.
        Err(error) => bmux_plugin_runtime::rt::events::RemoteEvents::current()
            .map(EventBus::remote)
            .ok_or(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_sdk::PluginEventKind;
    use serde::Serializer;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    struct SampleEvent {
        payload: u32,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
    struct OtherEvent {
        value: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct NonSerializeEvent {
        payload: u32,
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
    struct FocusSnapshot {
        focused: Option<u64>,
        revision: u64,
    }

    static JSON_EVENT_SERIALIZE_COUNT: AtomicUsize = AtomicUsize::new(0);
    static JSON_STATE_SERIALIZE_COUNT: AtomicUsize = AtomicUsize::new(0);

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct CountingEvent {
        value: u64,
    }

    impl Serialize for CountingEvent {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            JSON_EVENT_SERIALIZE_COUNT.fetch_add(1, Ordering::SeqCst);
            serializer.serialize_u64(self.value)
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct CountingSnapshot {
        value: u64,
    }

    impl Serialize for CountingSnapshot {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            JSON_STATE_SERIALIZE_COUNT.fetch_add(1, Ordering::SeqCst);
            serializer.serialize_u64(self.value)
        }
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
    async fn non_json_broadcast_channels_do_not_require_serialize() {
        let bus = EventBus::new();
        bus.register_channel::<NonSerializeEvent>(TEST_IFACE);
        let mut subscriber = bus.subscribe::<NonSerializeEvent>(&TEST_IFACE).unwrap();

        bus.emit(&TEST_IFACE, NonSerializeEvent { payload: 42 })
            .unwrap();

        let received = subscriber.recv().await.expect("should receive event");
        assert_eq!(received.as_ref(), &NonSerializeEvent { payload: 42 });
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

    #[tokio::test]
    async fn broadcast_json_subscriber_receives_serialized_payload() {
        let bus = EventBus::new();
        bus.register_channel_with_json_projection::<SampleEvent>(TEST_IFACE);
        let mut rx = bus.subscribe_json(&TEST_IFACE).unwrap();

        bus.emit(&TEST_IFACE, SampleEvent { payload: 99 }).unwrap();

        let event = rx.recv().await.unwrap();
        assert_eq!(event.interface, TEST_IFACE);
        assert_eq!(event.delivery, DeliveryMode::Broadcast);
        assert_eq!(event.payload["payload"], 99);
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

    #[test]
    fn independent_roots_never_share_channels() {
        let a = Arc::new(EventBus::new());
        let b = Arc::new(EventBus::new());
        assert!(!a.shares_channels_with(&b));
        a.register_channel::<SampleEvent>(PluginEventKind::from_static("test.independent"));
        assert!(
            b.subscribe::<SampleEvent>(&PluginEventKind::from_static("test.independent"))
                .is_err()
        );
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
    async fn state_channel_without_json_projection_rejects_json_subscriber() {
        let bus = EventBus::new();
        bus.register_state_channel::<FocusSnapshot>(
            STATE_IFACE,
            FocusSnapshot {
                focused: None,
                revision: 0,
            },
        );
        let err = bus.subscribe_state_json(&STATE_IFACE).unwrap_err();
        assert!(matches!(
            err,
            EventBusError::JsonProjectionUnavailable { .. }
        ));
    }

    #[tokio::test]
    async fn lazy_json_projection_serializes_state_only_when_observed() {
        JSON_STATE_SERIALIZE_COUNT.store(0, Ordering::SeqCst);
        let bus = EventBus::new();
        bus.register_state_channel_with_json_projection::<CountingSnapshot>(
            STATE_IFACE,
            CountingSnapshot { value: 0 },
        );
        assert_eq!(JSON_STATE_SERIALIZE_COUNT.load(Ordering::SeqCst), 0);

        bus.publish_state(&STATE_IFACE, CountingSnapshot { value: 1 })
            .unwrap();
        assert_eq!(JSON_STATE_SERIALIZE_COUNT.load(Ordering::SeqCst), 0);

        let (initial, mut rx) = bus.subscribe_state_json(&STATE_IFACE).unwrap();
        assert_eq!(initial.payload, JsonValue::from(1));
        assert_eq!(JSON_STATE_SERIALIZE_COUNT.load(Ordering::SeqCst), 1);

        bus.publish_state(&STATE_IFACE, CountingSnapshot { value: 2 })
            .unwrap();
        rx.changed().await.unwrap();
        assert_eq!(rx.borrow().payload, JsonValue::from(2));
        assert_eq!(JSON_STATE_SERIALIZE_COUNT.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn lazy_json_projection_serializes_broadcast_only_when_observed() {
        JSON_EVENT_SERIALIZE_COUNT.store(0, Ordering::SeqCst);
        let bus = EventBus::new();
        bus.register_channel_with_json_projection::<CountingEvent>(TEST_IFACE);

        bus.emit(&TEST_IFACE, CountingEvent { value: 1 }).unwrap();
        assert_eq!(JSON_EVENT_SERIALIZE_COUNT.load(Ordering::SeqCst), 0);

        let mut rx = bus.subscribe_json(&TEST_IFACE).unwrap();
        bus.emit(&TEST_IFACE, CountingEvent { value: 2 }).unwrap();
        let event = rx.recv().await.unwrap();
        assert_eq!(event.payload, JsonValue::from(2));
        assert_eq!(JSON_EVENT_SERIALIZE_COUNT.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn state_json_subscriber_receives_initial_and_live_payloads() {
        let bus = EventBus::new();
        bus.register_state_channel_with_json_projection::<FocusSnapshot>(
            STATE_IFACE,
            FocusSnapshot {
                focused: None,
                revision: 0,
            },
        );
        let (initial, mut rx) = bus.subscribe_state_json(&STATE_IFACE).unwrap();
        assert_eq!(initial.delivery, DeliveryMode::State);
        assert_eq!(initial.payload["revision"], 0);

        bus.publish_state(
            &STATE_IFACE,
            FocusSnapshot {
                focused: Some(12),
                revision: 1,
            },
        )
        .unwrap();

        rx.changed().await.unwrap();
        let event = rx.borrow().clone();
        assert_eq!(event.payload["focused"], 12);
        assert_eq!(event.payload["revision"], 1);
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

    #[tokio::test]
    async fn registration_wait_is_event_driven_and_validates_delivery() {
        use std::future::Future;
        let bus = EventBus::new();
        let kind = TEST_IFACE;
        let waiting = bus.subscribe_when_registered::<SampleEvent>(&kind);
        tokio::pin!(waiting);
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                waiting.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        bus.register_channel::<SampleEvent>(kind.clone());
        let _receiver = waiting.await.unwrap();
        bus.register_state_channel(kind.clone(), 1_u64);
        assert!(matches!(
            bus.subscribe_when_registered::<u64>(&kind).await,
            Err(EventBusError::ChannelDeliveryMismatch { .. })
        ));
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

    // ── Activation-scoped views ─────────────────────────────────────

    fn activation() -> bmux_plugin_runtime::ActivationScope {
        let instance = bmux_plugin_runtime::InstanceScope::new(
            bmux_plugin_runtime::PluginRuntimeScopeId::from_uuid(uuid::Uuid::new_v4()),
        );
        instance
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .expect("activation")
    }

    #[tokio::test]
    async fn scoped_view_shares_channels_and_releases_its_registrations_on_drop() {
        let root = Arc::new(EventBus::new());
        let view = root.scoped(activation());
        assert!(view.shares_channels_with(&root));
        view.register_channel::<SampleEvent>(TEST_IFACE);
        let mut sub = root.subscribe::<SampleEvent>(&TEST_IFACE).unwrap();
        view.emit(&TEST_IFACE, SampleEvent { payload: 3 }).unwrap();
        assert_eq!(sub.recv().await.unwrap().payload, 3);
        drop(view);
        assert!(matches!(
            root.subscribe::<SampleEvent>(&TEST_IFACE),
            Err(EventBusError::ChannelNotRegistered { .. })
        ));
        assert!(sub.recv().await.is_err(), "owner drop closes the channel");
    }

    #[test]
    fn closed_scope_rejects_operations_and_releases_registrations() {
        let scope = activation();
        let root = Arc::new(EventBus::new());
        let view = root.scoped(scope.clone());
        view.register_state_channel::<FocusSnapshot>(STATE_IFACE, FocusSnapshot::default());
        scope.close().unwrap();
        assert!(matches!(
            view.publish_state(&STATE_IFACE, FocusSnapshot::default()),
            Err(EventBusError::Closed)
        ));
        assert!(matches!(
            view.subscribe_state::<FocusSnapshot>(&STATE_IFACE),
            Err(EventBusError::Closed)
        ));
        assert!(root.subscribe_state::<FocusSnapshot>(&STATE_IFACE).is_err());
        view.register_channel::<SampleEvent>(TEST_IFACE);
        assert!(
            root.subscribe::<SampleEvent>(&TEST_IFACE).is_err(),
            "a closed view admits no registration"
        );
    }

    #[test]
    fn owner_never_removes_a_newer_registration() {
        let root = Arc::new(EventBus::new());
        let first = root.scoped(activation());
        let second = root.scoped(activation());
        first.register_channel::<SampleEvent>(TEST_IFACE);
        second.register_channel::<SampleEvent>(TEST_IFACE);
        drop(first);
        assert!(root.subscribe::<SampleEvent>(&TEST_IFACE).is_ok());
        drop(second);
        assert!(root.subscribe::<SampleEvent>(&TEST_IFACE).is_err());
    }

    #[test]
    fn activation_binder_offers_each_activation_its_own_view() {
        let root = Arc::new(EventBus::new());
        let binder = activation_binder(Arc::clone(&root));
        let take = |scope: &bmux_plugin_runtime::ActivationScope| {
            let capabilities = ActivationCapabilities::default();
            binder(scope, &capabilities).unwrap();
            let bus = take_activation_event_bus(&capabilities).unwrap();
            assert!(take_activation_event_bus(&capabilities).is_err());
            bus
        };
        let (a, b) = (take(&activation()), take(&activation()));
        assert!(!Arc::ptr_eq(&a, &b));
        assert!(a.shares_channels_with(&root) && b.shares_channels_with(&root));
        assert!(take_activation_event_bus(&ActivationCapabilities::default()).is_err());
    }
}
