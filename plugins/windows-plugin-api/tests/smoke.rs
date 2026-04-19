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

#[test]
fn event_kind_constant_is_namespaced_by_plugin_id() {
    // `plugin bmux.windows` in the BPDL source means every interface's
    // event stream is namespaced under `bmux.windows/<interface-name>`.
    assert_eq!(windows_events::EVENT_KIND, "bmux.windows/windows-events");
}

#[test]
fn event_payload_alias_matches_declared_type() {
    // The generated `EventPayload` alias must resolve to the same
    // `PaneEvent` variant the BPDL declared. Constructing one via the
    // alias round-trips through JSON identically to the direct type.
    let via_alias: windows_events::EventPayload = windows_events::EventPayload::Focused {
        pane_id: uuid::Uuid::nil(),
    };
    let via_type: PaneEvent = PaneEvent::Focused {
        pane_id: uuid::Uuid::nil(),
    };
    assert_eq!(
        serde_json::to_string(&via_alias).unwrap(),
        serde_json::to_string(&via_type).unwrap()
    );
}
