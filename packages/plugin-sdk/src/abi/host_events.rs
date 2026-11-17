//! Wire encoding of serialized activation events across plug-rt.
//!
//! Each event crosses as one JSON object `{"state": bool, "payload": ...}`:
//! whether it came from a state channel, and its JSON payload. The host
//! encodes, the plugin decodes; channel keys travel separately.
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

#[derive(Serialize, Deserialize)]
struct WireEvent {
    state: bool,
    payload: JsonValue,
}

/// Encode one event; `None` when the payload cannot be serialized.
#[must_use]
pub fn encode_event(state: bool, payload: &JsonValue) -> Option<Vec<u8>> {
    serde_json::to_vec(&WireEvent {
        state,
        payload: payload.clone(),
    })
    .ok()
}

/// Decode one event into `(state, payload)`; `None` when malformed.
#[must_use]
pub fn decode_event(bytes: &[u8]) -> Option<(bool, JsonValue)> {
    let wire: WireEvent = serde_json::from_slice(bytes).ok()?;
    Some((wire.state, wire.payload))
}

/// A plugin-side subscription over the serialized event port, yielding each
/// event's JSON payload.
pub(crate) struct RemoteEventStream(pub(crate) bmux_plugin_runtime::rt::events::RemoteSubscription);
impl crate::HostEventStream for RemoteEventStream {
    fn poll_next(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Vec<u8>>> {
        loop {
            match self.0.poll_next(cx) {
                std::task::Poll::Pending => return std::task::Poll::Pending,
                std::task::Poll::Ready(None) => return std::task::Poll::Ready(None),
                std::task::Poll::Ready(Some(bytes)) => {
                    if let Some((_, payload)) = decode_event(&bytes)
                        && let Ok(payload) = serde_json::to_vec(&payload)
                    {
                        return std::task::Poll::Ready(Some(payload));
                    }
                    // Malformed item: skip it.
                }
            }
        }
    }
}
