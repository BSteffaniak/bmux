use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::BTreeSet;

use crate::{PluginError, PluginEventKind, Result};

/// A published plugin event.
///
/// Events in bmux are fully owned by plugins: the publisher declares an
/// `events <type>` stream in its BPDL schema and the generated plugin-api
/// crate exposes a `pub const EVENT_KIND: PluginEventKind` that both the
/// publisher and any subscriber reference. Core relays the envelope
/// without interpreting the kind; on the wire it is a plain string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEvent {
    /// Canonical identifier for the event stream this event belongs to
    /// (for example `"bmux.windows/pane-event"`).
    pub kind: PluginEventKind,
    /// Serialized event payload. Decoders interpret this according to
    /// the type the owning plugin declared for the stream.
    #[serde(default)]
    pub payload: PluginEventPayload,
}

impl PluginEvent {
    /// Construct a typed plugin event. `kind` should be the
    /// BPDL-generated [`PluginEventKind`] constant for this event
    /// stream; the typed payload is serialized via serde.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::ServiceProtocol`] if the typed payload
    /// fails to serialize.
    pub fn typed<T: Serialize>(kind: PluginEventKind, value: &T) -> Result<Self> {
        let payload = serde_json::to_value(value).map_err(|err| PluginError::ServiceProtocol {
            details: format!("typed event serialize: {err}"),
        })?;
        Ok(Self { kind, payload })
    }

    /// Decode a typed event's payload into a concrete type.
    ///
    /// Returns `None` if the event's [`PluginEvent::kind`] does not
    /// match `expected_kind`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::ServiceProtocol`] if the payload exists
    /// and matches the expected kind but fails to deserialize into `T`.
    pub fn decode_typed<T: DeserializeOwned>(
        &self,
        expected_kind: &PluginEventKind,
    ) -> Result<Option<T>> {
        if self.kind != *expected_kind {
            return Ok(None);
        }
        let value: T = serde_json::from_value(self.payload.clone()).map_err(|err| {
            PluginError::ServiceProtocol {
                details: format!("typed event deserialize: {err}"),
            }
        })?;
        Ok(Some(value))
    }
}

pub type PluginEventPayload = Value;

/// A subscription filter that matches published events by kind.
///
/// An empty [`PluginEventSubscription`] matches every event. When
/// `kinds` is non-empty, only events whose [`PluginEvent::kind`] is in
/// the set match.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PluginEventSubscription {
    #[serde(default)]
    pub kinds: BTreeSet<PluginEventKind>,
}

impl PluginEventSubscription {
    /// A subscription that matches every event.
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }

    /// A subscription that matches only events with the given kind.
    #[must_use]
    pub fn for_kind(kind: PluginEventKind) -> Self {
        let mut kinds = BTreeSet::new();
        kinds.insert(kind);
        Self { kinds }
    }

    /// A subscription that matches any of the provided kinds.
    #[must_use]
    pub fn for_kinds<I>(kinds: I) -> Self
    where
        I: IntoIterator<Item = PluginEventKind>,
    {
        Self {
            kinds: kinds.into_iter().collect(),
        }
    }

    #[must_use]
    pub fn matches(&self, event: &PluginEvent) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&event.kind)
    }
}

/// Delivery semantics for a published plugin event kind.
///
/// Distinguishes transient broadcast events from reactive state
/// channels. The server's forwarder needs to know which mode the
/// plugin publishes under so it subscribes via the matching API
/// ([`crate::EventBus::subscribe`] vs `subscribe_state`). The
/// plugin-manifest loader surfaces this as an opt-in string
/// (`"broadcast"` or `"state"`); missing values default to
/// [`PluginEventDelivery::Broadcast`] to match the historical
/// behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PluginEventDelivery {
    /// Transient broadcast — every emission fans out to every live
    /// subscriber exactly once. Late subscribers miss prior events.
    /// Corresponds to `events <type>;` in the BPDL schema.
    #[default]
    Broadcast,
    /// Reactive state — the last published value is retained and
    /// replayed synchronously to new subscribers. Corresponds to
    /// `@state events <type>;` in the BPDL schema.
    State,
}

/// A declaration that a plugin publishes events of a given kind, with
/// hints about how the host should route them.
///
/// Plugins list publications in their `plugin.toml` so the host runtime
/// can wire up routing without scanning BPDL schemas at runtime. The
/// primary consumer today is the server's plugin-bus forwarder, which
/// propagates emissions over the wire to streaming clients when
/// `forward_to_streaming_clients` is true.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEventPublication {
    /// Canonical event kind the plugin emits.
    pub kind: PluginEventKind,
    /// When true, the server forwards every emission of `kind` onto
    /// each connected streaming client's event channel. Clients
    /// subscribe via the normal `ServerEvent::PluginBusEvent` arm and
    /// decode the payload using the plugin's typed schema. Leave this
    /// false for events that are purely intra-server (plugin-to-plugin)
    /// to avoid paying the IPC cost.
    #[serde(default)]
    pub forward_to_streaming_clients: bool,
    /// Delivery mode — `"broadcast"` (default) for transient events,
    /// `"state"` for reactive state channels. The server's forwarder
    /// must pick the matching subscription API; a mismatch would
    /// return [`crate::EventBusError::ChannelDeliveryMismatch`] at
    /// spawn time.
    #[serde(default)]
    pub delivery: PluginEventDelivery,
}

impl PluginEventPublication {
    /// Construct a broadcast publication entry for `kind` with
    /// forwarding on.
    #[must_use]
    pub const fn streaming(kind: PluginEventKind) -> Self {
        Self {
            kind,
            forward_to_streaming_clients: true,
            delivery: PluginEventDelivery::Broadcast,
        }
    }

    /// Construct a state-channel publication entry for `kind` with
    /// forwarding on.
    #[must_use]
    pub const fn streaming_state(kind: PluginEventKind) -> Self {
        Self {
            kind,
            forward_to_streaming_clients: true,
            delivery: PluginEventDelivery::State,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KIND: PluginEventKind = PluginEventKind::from_static("test.fixture/sample-event");
    const OTHER_KIND: PluginEventKind = PluginEventKind::from_static("test.fixture/other-event");

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct SampleEvent {
        item_id: u64,
        action: String,
    }

    #[test]
    fn typed_event_round_trips_through_payload() {
        let event = SampleEvent {
            item_id: 7,
            action: "focused".into(),
        };
        let envelope = PluginEvent::typed(TEST_KIND, &event).expect("encode");
        assert_eq!(envelope.kind, TEST_KIND);

        let decoded: SampleEvent = envelope
            .decode_typed(&TEST_KIND)
            .expect("decode")
            .expect("match");
        assert_eq!(decoded, event);
    }

    #[test]
    fn decode_typed_rejects_wrong_kind() {
        let event = SampleEvent {
            item_id: 1,
            action: "focused".into(),
        };
        let envelope = PluginEvent::typed(TEST_KIND, &event).expect("encode");

        let result: Option<SampleEvent> = envelope
            .decode_typed(&OTHER_KIND)
            .expect("decode returns ok");
        assert!(
            result.is_none(),
            "event with different kind should not decode"
        );
    }

    #[test]
    fn subscription_for_kind_matches_only_that_kind() {
        let sub = PluginEventSubscription::for_kind(TEST_KIND);

        let matching = PluginEvent {
            kind: TEST_KIND,
            payload: serde_json::json!({}),
        };
        assert!(sub.matches(&matching));

        let other = PluginEvent {
            kind: OTHER_KIND,
            payload: serde_json::json!({}),
        };
        assert!(!sub.matches(&other));
    }

    #[test]
    fn empty_subscription_matches_everything() {
        let sub = PluginEventSubscription::all();
        let event = PluginEvent {
            kind: TEST_KIND,
            payload: serde_json::json!({}),
        };
        assert!(sub.matches(&event));
    }
}
