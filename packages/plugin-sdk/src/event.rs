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
