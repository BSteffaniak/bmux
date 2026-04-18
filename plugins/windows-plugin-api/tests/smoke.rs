//! Smoke test: the BPDL-generated bindings compile and their types can
//! be constructed, serialized, and used via the trait signature.

use bmux_windows_plugin_api::{
    windows_events::{self, PaneEvent},
    windows_state::{self, PaneState, PaneStatus},
};

#[test]
fn pane_state_record_round_trips() {
    let state = PaneState {
        id: uuid::Uuid::nil(),
        session_id: uuid::Uuid::nil(),
        focused: true,
        zoomed: false,
        name: Some("main".to_string()),
        status: PaneStatus::Running,
    };
    let json = serde_json::to_string(&state).expect("serialize");
    let round: PaneState = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(state, round);
}

#[test]
fn pane_status_variant_payload_serializes_case() {
    let exited = PaneStatus::Exited { exit_code: 42 };
    let json = serde_json::to_string(&exited).expect("serialize");
    assert!(json.contains("exited"));
    assert!(json.contains("42"));
}

#[test]
fn pane_event_variant_is_tagged() {
    let ev = PaneEvent::Focused {
        pane_id: uuid::Uuid::nil(),
    };
    let json = serde_json::to_string(&ev).expect("serialize");
    assert!(json.contains("\"kind\":\"focused\""));
}

#[test]
fn interface_ids_match_bpdl_source() {
    assert_eq!(windows_state::INTERFACE_ID, "windows-state");
    assert_eq!(windows_events::INTERFACE_ID, "windows-events");
}
