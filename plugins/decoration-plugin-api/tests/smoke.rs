//! Smoke test: the BPDL-generated decoration bindings compile and
//! round-trip through serde.

use bmux_decoration_plugin_api::decoration_state::{
    BorderStyle, DecorationEvent, NotifyError, PaneActivity, PaneDecoration, PaneEvent,
    PaneGeometry, PaneLifecycle, SetStyleError,
};
use bmux_scene_protocol::scene_protocol::Rect;

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

// ─── PR 2 additions ───────────────────────────────────────────

#[test]
fn pane_geometry_round_trips() {
    let g = PaneGeometry {
        pane_id: uuid::Uuid::from_u128(1),
        rect: Rect {
            x: 0,
            y: 1,
            w: 20,
            h: 5,
        },
        content_rect: Rect {
            x: 1,
            y: 2,
            w: 18,
            h: 3,
        },
    };
    let json = serde_json::to_string(&g).expect("serialize");
    let round: PaneGeometry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(g, round);
}

#[test]
fn pane_activity_round_trips() {
    let a = PaneActivity {
        pane_id: uuid::Uuid::from_u128(2),
        focused: true,
        zoomed: false,
        status: PaneLifecycle::Running,
    };
    let json = serde_json::to_string(&a).expect("serialize");
    let round: PaneActivity = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(a, round);
}

#[test]
fn pane_lifecycle_default_is_running() {
    assert_eq!(PaneLifecycle::default(), PaneLifecycle::Running);
}

#[test]
fn pane_event_variants_are_tagged() {
    let events = [
        PaneEvent::Focused {
            pane_id: uuid::Uuid::from_u128(1),
        },
        PaneEvent::Unfocused {
            pane_id: uuid::Uuid::from_u128(2),
        },
        PaneEvent::Zoomed {
            pane_id: uuid::Uuid::from_u128(3),
        },
        PaneEvent::Unzoomed {
            pane_id: uuid::Uuid::from_u128(4),
        },
        PaneEvent::Opened {
            pane_id: uuid::Uuid::from_u128(5),
            session_id: uuid::Uuid::from_u128(6),
        },
        PaneEvent::Closed {
            pane_id: uuid::Uuid::from_u128(7),
        },
        PaneEvent::StatusChanged {
            pane_id: uuid::Uuid::from_u128(8),
            exited: true,
        },
    ];
    for e in events {
        let json = serde_json::to_string(&e).expect("encode");
        let round: PaneEvent = serde_json::from_str(&json).expect("decode");
        assert_eq!(e, round);
    }
}

#[test]
fn notify_error_invalid_argument_serializes() {
    let err = NotifyError::InvalidArgument {
        reason: "pane not tracked".into(),
    };
    let json = serde_json::to_string(&err).expect("serialize");
    assert!(json.contains("\"invalid_argument\""));
    assert!(json.contains("pane not tracked"));
}

#[test]
fn pane_state_changed_decoration_event_is_tagged() {
    let ev = DecorationEvent::PaneStateChanged {
        pane_id: uuid::Uuid::from_u128(9),
    };
    let json = serde_json::to_string(&ev).expect("serialize");
    assert!(json.contains("\"pane_state_changed\""));
}
