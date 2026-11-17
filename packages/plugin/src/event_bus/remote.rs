//! The event bus across the dynamic-library boundary.
//!
//! The host serves an activation's scoped bus view as a serialized event port
//! ([`BusScopeEvents`], bound per instance through the plug-rt table). Inside
//! a dynamic plugin a remote [`EventBus`] forwards the serialized surface to
//! that port. Every item crosses as one JSON [`WireEvent`]. Subscriptions are
//! host resources of the instance scope: they are polled with real wakers by
//! a plug-rt task that the activation owns, and released with the activation.
use super::{
    DEFAULT_EVENT_BUS_CAPACITY, DeliveryMode, EventBus, EventBusBytesError, EventBusError,
    EventBusResult, JsonPluginEvent,
};
use bmux_plugin_runtime::rt::{
    self,
    events::{EventPortError, EventStream, RemoteEvents, ScopeEvents},
};
use bmux_plugin_sdk::PluginEventKind;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::sync::Arc;
use tokio::sync::{broadcast, watch};

/// One serialized event.
#[derive(Debug, Serialize, Deserialize)]
struct WireEvent {
    state: bool,
    payload: JsonValue,
}

fn encode(event: &JsonPluginEvent) -> Option<Vec<u8>> {
    serde_json::to_vec(&WireEvent {
        state: event.delivery == DeliveryMode::State,
        payload: event.payload.clone(),
    })
    .ok()
}

fn decode(interface: &PluginEventKind, bytes: &[u8]) -> Option<JsonPluginEvent> {
    let wire: WireEvent = serde_json::from_slice(bytes).ok()?;
    Some(JsonPluginEvent {
        interface: interface.clone(),
        delivery: if wire.state {
            DeliveryMode::State
        } else {
            DeliveryMode::Broadcast
        },
        payload: wire.payload,
    })
}

pub(super) fn port_error(error: EventPortError) -> EventBusError {
    match error {
        EventPortError::Closed => EventBusError::Closed,
        other => EventBusError::Remote(other.to_string()),
    }
}

/// The host refusing serialized input means no channel accepts it, which an
/// in-process bus reports as `Ok(false)`.
pub(super) fn publish(
    remote: &RemoteEvents,
    interface: &PluginEventKind,
    payload: &[u8],
) -> Result<bool, EventBusBytesError> {
    match remote.publish(interface.as_str(), payload) {
        Ok(()) => Ok(true),
        Err(EventPortError::Rejected(_)) => Ok(false),
        Err(error) => Err(EventBusBytesError::Bus(port_error(error))),
    }
}

/// Spawn the forwarder inside the port's instance scope, so the activation
/// owns it and teardown stops it even when the caller is outside the scope.
fn forward<F>(remote: &RemoteEvents, name: &str, forwarder: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let _scope = rt::enter_instance(remote.scope());
    drop(rt::spawn_named(name, forwarder));
}

pub(super) fn subscribe_json(
    remote: &RemoteEvents,
    interface: &PluginEventKind,
) -> EventBusResult<broadcast::Receiver<Arc<JsonPluginEvent>>> {
    let mut subscription = remote
        .subscribe(interface.as_str(), false)
        .map_err(port_error)?;
    let (sender, receiver) = broadcast::channel(DEFAULT_EVENT_BUS_CAPACITY);
    let kind = interface.clone();
    forward(remote, "event-bus.remote-subscription", async move {
        loop {
            tokio::select! {
                // Every local receiver is gone: release the host subscription.
                () = sender.closed() => return,
                next = subscription.next() => {
                    let Some(bytes) = next else { return };
                    if let Some(event) = decode(&kind, &bytes) {
                        let _ = sender.send(Arc::new(event));
                    }
                }
            }
        }
    });
    Ok(receiver)
}

pub(super) fn subscribe_state_json(
    remote: &RemoteEvents,
    interface: &PluginEventKind,
) -> EventBusResult<(Arc<JsonPluginEvent>, watch::Receiver<Arc<JsonPluginEvent>>)> {
    let mut subscription = remote
        .subscribe(interface.as_str(), true)
        .map_err(port_error)?;
    // A state stream yields its retained value immediately.
    let first = match subscription
        .poll_next(&mut std::task::Context::from_waker(std::task::Waker::noop()))
    {
        std::task::Poll::Ready(Some(bytes)) => decode(interface, &bytes),
        _ => None,
    }
    .ok_or_else(|| EventBusError::Remote("state channel delivered no current value".into()))?;
    let current = Arc::new(first);
    let (sender, receiver) = watch::channel(Arc::clone(&current));
    let kind = interface.clone();
    forward(remote, "event-bus.remote-state", async move {
        loop {
            tokio::select! {
                () = sender.closed() => return,
                next = subscription.next() => {
                    let Some(bytes) = next else { return };
                    if let Some(event) = decode(&kind, &bytes) {
                        sender.send_replace(Arc::new(event));
                    }
                }
            }
        }
    });
    Ok((current, receiver))
}

/// The host side: one activation's scoped bus view served as a serialized
/// event port for a dynamic plugin instance.
pub struct BusScopeEvents(pub Arc<EventBus>);

fn host_error(error: &EventBusError) -> EventPortError {
    match error {
        EventBusError::Closed => EventPortError::Closed,
        other => EventPortError::Rejected(other.to_string()),
    }
}

impl ScopeEvents for BusScopeEvents {
    fn register(&self, kind: &str) -> Result<(), EventPortError> {
        self.0
            .register_json_channel(PluginEventKind::from(kind.to_string()))
            .map_err(|error| host_error(&error))
    }

    fn publish(&self, kind: &str, payload: &[u8]) -> Result<(), EventPortError> {
        match self
            .0
            .emit_from_bytes(&PluginEventKind::from(kind.to_string()), payload)
        {
            Ok(true) => Ok(()),
            Ok(false) => Err(EventPortError::Rejected(format!(
                "no channel `{kind}` accepts serialized payloads"
            ))),
            Err(EventBusBytesError::Bus(error)) => Err(host_error(&error)),
            Err(error) => Err(EventPortError::Rejected(error.to_string())),
        }
    }

    fn subscribe(&self, kind: &str, state: bool) -> Result<Box<dyn EventStream>, EventPortError> {
        let kind = PluginEventKind::from(kind.to_string());
        if state {
            let (_, receiver) = self
                .0
                .subscribe_state_json(&kind)
                .map_err(|error| host_error(&error))?;
            Ok(rt::events::watch_stream(receiver, |event| encode(&event)))
        } else {
            let receiver = self
                .0
                .subscribe_json(&kind)
                .map_err(|error| host_error(&error))?;
            Ok(rt::events::broadcast_stream(receiver, |event| {
                encode(&event)
            }))
        }
    }
}
