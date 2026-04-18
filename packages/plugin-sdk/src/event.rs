use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::BTreeSet;

use crate::{PluginError, Result};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginEventKind {
    System,
    Session,
    Window,
    Pane,
    Client,
    Command,
    Terminal,
    Custom,
    /// A typed plugin-to-plugin event emitted through a BPDL-declared
    /// `events <type>` stream. The [`PluginEvent::name`] carries the
    /// canonical interface id that declared the stream; the
    /// [`PluginEvent::payload`] is the JSON-serialized typed event.
    Typed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEvent {
    pub kind: PluginEventKind,
    pub name: String,
    #[serde(default)]
    pub payload: PluginEventPayload,
}

impl PluginEvent {
    /// Construct a typed plugin event. `interface_id` should match the
    /// BPDL interface that declared the event stream (for example,
    /// `"windows-events"`). The typed payload is serialized via serde.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::ServiceProtocol`] if the typed payload
    /// fails to serialize.
    pub fn typed<T: Serialize>(interface_id: impl Into<String>, value: &T) -> Result<Self> {
        let payload = serde_json::to_value(value).map_err(|err| PluginError::ServiceProtocol {
            details: format!("typed event serialize: {err}"),
        })?;
        Ok(Self {
            kind: PluginEventKind::Typed,
            name: interface_id.into(),
            payload,
        })
    }

    /// Decode a typed event's payload into a concrete type.
    ///
    /// Returns `None` if the event is not a typed event or if its
    /// [`PluginEvent::name`] doesn't match the expected interface id.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::ServiceProtocol`] if the payload exists
    /// and matches the expected interface but fails to deserialize
    /// into `T`.
    pub fn decode_typed<T: DeserializeOwned>(&self, interface_id: &str) -> Result<Option<T>> {
        if self.kind != PluginEventKind::Typed || self.name != interface_id {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEventSubscription {
    #[serde(default)]
    pub kinds: BTreeSet<PluginEventKind>,
    #[serde(default)]
    pub names: BTreeSet<String>,
}

impl PluginEventSubscription {
    #[must_use]
    pub fn matches(&self, event: &PluginEvent) -> bool {
        let kind_matches = self.kinds.is_empty() || self.kinds.contains(&event.kind);
        let name_matches = self.names.is_empty() || self.names.contains(&event.name);
        kind_matches && name_matches
    }

    /// Build a subscription targeting one typed event stream by its
    /// interface id. Matches only [`PluginEventKind::Typed`] events
    /// with a matching [`PluginEvent::name`].
    #[must_use]
    pub fn typed(interface_id: impl Into<String>) -> Self {
        let mut kinds = BTreeSet::new();
        kinds.insert(PluginEventKind::Typed);
        let mut names = BTreeSet::new();
        names.insert(interface_id.into());
        Self { kinds, names }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct SampleEvent {
        pane_id: u64,
        kind: String,
    }

    #[test]
    fn typed_event_round_trips_through_payload() {
        let event = SampleEvent {
            pane_id: 7,
            kind: "focused".into(),
        };
        let envelope = PluginEvent::typed("windows-events", &event).expect("encode");
        assert_eq!(envelope.kind, PluginEventKind::Typed);
        assert_eq!(envelope.name, "windows-events");

        let decoded: SampleEvent = envelope
            .decode_typed("windows-events")
            .expect("decode")
            .expect("match");
        assert_eq!(decoded, event);
    }

    #[test]
    fn decode_typed_rejects_wrong_interface() {
        let event = SampleEvent {
            pane_id: 1,
            kind: "focused".into(),
        };
        let envelope = PluginEvent::typed("windows-events", &event).expect("encode");

        let result: Option<SampleEvent> = envelope
            .decode_typed("decoration-events")
            .expect("decode returns ok");
        assert!(
            result.is_none(),
            "event with different interface id should not decode"
        );
    }

    #[test]
    fn typed_subscription_only_matches_matching_typed_events() {
        let sub = PluginEventSubscription::typed("windows-events");

        let matching = PluginEvent {
            kind: PluginEventKind::Typed,
            name: "windows-events".into(),
            payload: serde_json::json!({}),
        };
        assert!(sub.matches(&matching));

        let untyped = PluginEvent {
            kind: PluginEventKind::Window,
            name: "windows-events".into(),
            payload: serde_json::json!({}),
        };
        assert!(!sub.matches(&untyped));

        let wrong_iface = PluginEvent {
            kind: PluginEventKind::Typed,
            name: "decoration-events".into(),
            payload: serde_json::json!({}),
        };
        assert!(!sub.matches(&wrong_iface));
    }
}
