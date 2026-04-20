//! Smoke test: the BPDL-generated decoration bindings compile and
//! round-trip through serde.

use bmux_decoration_plugin_api::decoration_state::{
    BorderStyle, DecorationEvent, PaneDecoration, SetStyleError,
};

#[test]
fn pane_decoration_record_round_trips() {
    let value = PaneDecoration {
        pane_id: uuid::Uuid::nil(),
        border: BorderStyle::Ascii,
        focused: true,
        running_badge: Some("[RUNNING]".to_string()),
        exited_badge: Some("[EXITED]".to_string()),
    };
    let json = serde_json::to_string(&value).expect("serialize");
    let round: PaneDecoration = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(value, round);
}

#[test]
fn border_style_serializes_as_snake_case_tag() {
    let json = serde_json::to_string(&BorderStyle::Double).expect("serialize");
    assert_eq!(json, "\"double\"");
    let json = serde_json::to_string(&BorderStyle::None).expect("serialize");
    assert_eq!(json, "\"none\"");
}

#[test]
fn set_style_error_variant_payload_serializes() {
    let err = SetStyleError::StyleUnsupported {
        style: "retro".into(),
    };
    let json = serde_json::to_string(&err).expect("serialize");
    assert!(json.contains("\"style_unsupported\""));
    assert!(json.contains("retro"));
}

#[test]
fn decoration_event_is_tagged() {
    let ev = DecorationEvent::PaneRestyled {
        pane_id: uuid::Uuid::nil(),
    };
    let json = serde_json::to_string(&ev).expect("serialize");
    assert!(json.contains("\"pane_restyled\""));
}
