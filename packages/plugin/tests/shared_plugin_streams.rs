//! Real bounded event sinks: explicit backpressure, order, invocation ownership.
//! This does not claim dynamic checked-streaming negotiation is implemented.
use bmux_plugin_sdk::{
    BoundedServiceEventSink, PluginInvocationId, ServiceEvent, ServiceEventSink,
};

#[test]
fn stream_overflow_is_explicit_and_does_not_discard_accepted_events() {
    let invocation = PluginInvocationId::new();
    let sink = BoundedServiceEventSink::new(2);
    for payload in [b"first".to_vec(), b"second".to_vec()] {
        sink.emit_event(ServiceEvent {
            invocation_id: invocation.clone(),
            payload,
        })
        .unwrap();
    }
    assert!(
        sink.emit_event(ServiceEvent {
            invocation_id: invocation.clone(),
            payload: b"overflow".to_vec()
        })
        .is_err()
    );
    let events = sink.drain().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].payload, b"first");
    assert_eq!(events[1].payload, b"second");
    assert!(events.iter().all(|event| event.invocation_id == invocation));
    assert!(sink.drain().unwrap().is_empty());
    sink.emit_event(ServiceEvent {
        invocation_id: invocation.clone(),
        payload: b"after-drain".to_vec(),
    })
    .unwrap();
    assert_eq!(sink.drain().unwrap()[0].invocation_id, invocation);
}
#[test]
fn sibling_streams_have_independent_capacity_and_correlation() {
    let a = BoundedServiceEventSink::new(1);
    let b = BoundedServiceEventSink::new(1);
    let first = PluginInvocationId::new();
    let second = PluginInvocationId::new();
    assert_ne!(first, second);
    a.emit_event(ServiceEvent {
        invocation_id: first.clone(),
        payload: b"a".to_vec(),
    })
    .unwrap();
    b.emit_event(ServiceEvent {
        invocation_id: second.clone(),
        payload: b"b".to_vec(),
    })
    .unwrap();
    assert_eq!(a.drain().unwrap()[0].invocation_id, first);
    assert_eq!(b.drain().unwrap()[0].invocation_id, second);
}
#[test]
fn zero_capacity_is_rejection_not_a_noop_sink() {
    let sink = BoundedServiceEventSink::new(0);
    assert!(
        sink.emit_event(ServiceEvent {
            invocation_id: PluginInvocationId::new(),
            payload: b"must-not-disappear".to_vec(),
        })
        .is_err()
    );
    assert!(sink.drain().unwrap().is_empty());
}
