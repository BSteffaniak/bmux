//! Smoke test: the BPDL-generated contexts-plugin-api bindings compile
//! and their types can be constructed, serialized, and the declared
//! constants match the schema.

use bmux_contexts_plugin_api::{
    contexts_commands::{CloseContextError, ContextAck, CreateContextError},
    contexts_events::{self, ContextEvent},
    contexts_state::{self, ContextSelector, ContextSummary},
};

#[test]
fn context_summary_round_trips() {
    let mut attrs = std::collections::BTreeMap::new();
    attrs.insert("project".to_string(), "bmux".to_string());
    let c = ContextSummary {
        id: uuid::Uuid::nil(),
        name: Some("work".to_string()),
        attributes: attrs,
    };
    let json = serde_json::to_string(&c).expect("serialize");
    let round: ContextSummary = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(c, round);
}

#[test]
fn context_selector_allows_id_or_name() {
    let by_id = ContextSelector {
        id: Some(uuid::Uuid::nil()),
        name: None,
    };
    let by_name = ContextSelector {
        id: None,
        name: Some("work".to_string()),
    };
    let json_id = serde_json::to_string(&by_id).expect("serialize id");
    let json_name = serde_json::to_string(&by_name).expect("serialize name");
    assert!(json_id.contains("id"));
    assert!(json_name.contains("work"));
}

#[test]
fn context_ack_and_error_variants_serialize() {
    let ack = ContextAck {
        id: uuid::Uuid::nil(),
        session_id: None,
    };
    assert!(serde_json::to_string(&ack).expect("ack").contains("id"));

    // `CreateContextError` intentionally has no `NameAlreadyExists`
    // variant: context names are display hints, not identity. Two
    // contexts may share a name deliberately.
    let err = CreateContextError::InvalidName {
        reason: "empty".to_string(),
    };
    let json = serde_json::to_string(&err).expect("err");
    assert!(json.contains("invalid_name"));
    assert!(json.contains("empty"));

    let close_err = CloseContextError::NotFound;
    assert!(
        serde_json::to_string(&close_err)
            .expect("close_err")
            .contains("not_found")
    );
}

#[test]
fn context_event_variants_are_tagged() {
    let ev = ContextEvent::Created {
        context_id: uuid::Uuid::nil(),
        name: Some("work".to_string()),
    };
    let json = serde_json::to_string(&ev).expect("serialize");
    assert!(json.contains("\"created\""));
}

#[test]
fn interface_ids_match_bpdl_source() {
    assert_eq!(contexts_state::INTERFACE_ID, "contexts-state");
    assert_eq!(contexts_events::INTERFACE_ID, "contexts-events");
}

#[test]
fn event_kind_is_namespaced_by_plugin_id() {
    assert_eq!(contexts_events::EVENT_KIND, "bmux.contexts/contexts-events");
}

#[test]
fn event_payload_alias_matches_declared_type() {
    let via_alias: contexts_events::EventPayload = contexts_events::EventPayload::Closed {
        context_id: uuid::Uuid::nil(),
    };
    let via_type: ContextEvent = ContextEvent::Closed {
        context_id: uuid::Uuid::nil(),
    };
    assert_eq!(
        serde_json::to_string(&via_alias).unwrap(),
        serde_json::to_string(&via_type).unwrap()
    );
}

#[test]
fn session_active_context_changed_round_trips() {
    // Regression: the multi-client retarget broadcast carries
    // session id, context id, and the initiating client (None =
    // server-initiated) so attach runtimes can apply follow policy.
    let initiator = uuid::Uuid::from_u128(0x1234);
    let session = uuid::Uuid::from_u128(0x0A01);
    let context = uuid::Uuid::from_u128(0x0B02);
    let ev = ContextEvent::SessionActiveContextChanged {
        session_id: session,
        context_id: context,
        initiator_client_id: Some(initiator),
    };
    let json = serde_json::to_string(&ev).expect("serialize");
    assert!(json.contains("session_active_context_changed"), "{json}");
    assert!(json.contains(&session.to_string()), "{json}");
    assert!(json.contains(&context.to_string()), "{json}");
    assert!(json.contains(&initiator.to_string()), "{json}");
    let round: ContextEvent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(ev, round);
}

#[test]
fn session_active_context_changed_accepts_none_initiator() {
    let ev = ContextEvent::SessionActiveContextChanged {
        session_id: uuid::Uuid::nil(),
        context_id: uuid::Uuid::nil(),
        initiator_client_id: None,
    };
    let json = serde_json::to_string(&ev).expect("serialize");
    let round: ContextEvent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(ev, round);
}
