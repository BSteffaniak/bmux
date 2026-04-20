//! Smoke test: the BPDL-generated sessions-plugin-api bindings compile
//! and their types can be constructed, serialized, and the declared
//! constants match the schema.

use bmux_sessions_plugin_api::{
    sessions_commands::{NewSessionError, SessionAck},
    sessions_events::{self, SessionEvent},
    sessions_state::{self, SessionSelector, SessionSummary},
};

#[test]
fn session_summary_round_trips() {
    let s = SessionSummary {
        id: uuid::Uuid::nil(),
        name: Some("editor".to_string()),
        client_count: 2,
    };
    let json = serde_json::to_string(&s).expect("serialize");
    let round: SessionSummary = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(s, round);
}

#[test]
fn session_selector_allows_id_or_name() {
    let by_id = SessionSelector {
        id: Some(uuid::Uuid::nil()),
        name: None,
    };
    let by_name = SessionSelector {
        id: None,
        name: Some("editor".to_string()),
    };
    let json_id = serde_json::to_string(&by_id).expect("serialize id");
    let json_name = serde_json::to_string(&by_name).expect("serialize name");
    assert!(json_id.contains("id"));
    assert!(json_name.contains("editor"));
}

#[test]
fn session_ack_and_error_variants_serialize() {
    let ack = SessionAck {
        id: uuid::Uuid::nil(),
    };
    assert!(serde_json::to_string(&ack).expect("ack").contains("id"));

    let err = NewSessionError::NameAlreadyExists {
        name: "dup".to_string(),
    };
    let json = serde_json::to_string(&err).expect("err");
    assert!(json.contains("name_already_exists"));
    assert!(json.contains("dup"));
}

#[test]
fn session_event_variants_are_tagged() {
    let ev = SessionEvent::Created {
        session_id: uuid::Uuid::nil(),
        name: Some("editor".to_string()),
    };
    let json = serde_json::to_string(&ev).expect("serialize");
    assert!(json.contains("\"created\""));
}

#[test]
fn interface_ids_match_bpdl_source() {
    assert_eq!(sessions_state::INTERFACE_ID, "sessions-state");
    assert_eq!(sessions_events::INTERFACE_ID, "sessions-events");
}

#[test]
fn event_kind_is_namespaced_by_plugin_id() {
    // `plugin bmux.sessions` in the BPDL source means the event kind
    // is namespaced under `bmux.sessions/<interface-name>`.
    assert_eq!(sessions_events::EVENT_KIND, "bmux.sessions/sessions-events");
}

#[test]
fn event_payload_alias_matches_declared_type() {
    let via_alias: sessions_events::EventPayload = sessions_events::EventPayload::Removed {
        session_id: uuid::Uuid::nil(),
    };
    let via_type: SessionEvent = SessionEvent::Removed {
        session_id: uuid::Uuid::nil(),
    };
    assert_eq!(
        serde_json::to_string(&via_alias).unwrap(),
        serde_json::to_string(&via_type).unwrap()
    );
}
