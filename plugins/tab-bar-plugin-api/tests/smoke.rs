//! Smoke test: the BPDL-generated tab-bar bindings compile, keep their wire
//! identifiers, and round-trip through serde.

use bmux_tab_bar_plugin_api::{
    capabilities, notifications, notifications_list, presentation_actions, status_items,
    status_items_list, status_items_state,
};

#[test]
fn interface_ids_match_bpdl_source() {
    assert_eq!(
        presentation_actions::INTERFACE_ID,
        "presentation-actions/v2"
    );
    assert_eq!(status_items::INTERFACE_ID, "status-items");
    assert_eq!(status_items_state::INTERFACE_ID, "status-items-state");
    assert_eq!(status_items_list::INTERFACE_ID, "status-items-list");
    assert_eq!(notifications::INTERFACE_ID, "notifications");
    assert_eq!(notifications_list::INTERFACE_ID, "notifications-list");
}

#[test]
fn state_kinds_are_namespaced_by_plugin_id() {
    assert_eq!(
        status_items_list::STATE_KIND.as_str(),
        "bmux.tab_bar/status-items-list"
    );
    assert_eq!(
        notifications_list::STATE_KIND.as_str(),
        "bmux.tab_bar/notifications-list"
    );
}

#[test]
fn capabilities_match_bpdl_source() {
    assert_eq!(capabilities::INPUT.as_str(), "bmux.tab_bar.input");
    assert_eq!(
        capabilities::STATUS_READ.as_str(),
        "bmux.tab_bar.status.read"
    );
    assert_eq!(
        capabilities::STATUS_WRITE.as_str(),
        "bmux.tab_bar.status.write"
    );
    assert_eq!(capabilities::NOTIFY.as_str(), "bmux.tab_bar.notify");
}

#[test]
fn severity_defaults_to_normal_and_serializes_snake_case() {
    assert_eq!(
        status_items_list::Severity::default(),
        status_items_list::Severity::Normal
    );
    let json = serde_json::to_string(&status_items_list::Severity::Critical).unwrap();
    assert!(json.contains("critical"), "{json}");
}

#[test]
fn status_item_snapshot_round_trips() {
    let snapshot = status_items_list::StatusItemSnapshot {
        items: vec![status_items_list::StatusItem {
            owner: "example.native".into(),
            id: "build".into(),
            text: "build ok".into(),
            severity: status_items_list::Severity::Success,
            priority: 10,
            tooltip: Some("last build passed".into()),
        }],
        revision: 3,
    };
    let json = serde_json::to_string(&snapshot).unwrap();
    let round: status_items_list::StatusItemSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(round, snapshot);
}

#[test]
fn set_status_item_request_and_error_round_trip_through_service_codec() {
    let request = status_items::client::SetStatusItemRequest {
        id: "build".into(),
        text: "build ok".into(),
        severity: status_items_list::Severity::Warning,
        priority: -1,
        tooltip: None,
    };
    let bytes = bmux_plugin_sdk::encode_service_message(&request).unwrap();
    let decoded: status_items::client::SetStatusItemRequest =
        bmux_plugin_sdk::decode_service_message(&bytes).unwrap();
    assert_eq!(decoded.id, request.id);
    assert_eq!(decoded.text, request.text);
    assert_eq!(decoded.severity, request.severity);
    assert_eq!(decoded.priority, request.priority);
    assert_eq!(decoded.tooltip, request.tooltip);

    let error: Result<(), status_items::StatusItemError> =
        Err(status_items::StatusItemError::LimitExceeded { limit: 8 });
    let bytes = bmux_plugin_sdk::encode_service_message(&error).unwrap();
    let decoded: Result<(), status_items::StatusItemError> =
        bmux_plugin_sdk::decode_service_message(&bytes).unwrap();
    assert_eq!(decoded, error);
}

#[test]
fn notification_snapshot_round_trips() {
    let snapshot = notifications_list::NotificationSnapshot {
        notifications: vec![notifications_list::Notification {
            id: uuid::Uuid::from_u128(7),
            owner: "example.native".into(),
            title: "Deploy".into(),
            body: Some("finished".into()),
            severity: status_items_list::Severity::Info,
            created_at_ms: 1_700_000_000_000,
            ttl_ms: 5_000,
        }],
        revision: 1,
    };
    let json = serde_json::to_string(&snapshot).unwrap();
    let round: notifications_list::NotificationSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(round, snapshot);
    let request = notifications::client::NotifyRequest {
        title: "Deploy".into(),
        body: None,
        severity: status_items_list::Severity::Normal,
        ttl_ms: Some(1_000),
    };
    let bytes = bmux_plugin_sdk::encode_service_message(&request).unwrap();
    let decoded: notifications::client::NotifyRequest =
        bmux_plugin_sdk::decode_service_message(&bytes).unwrap();
    assert_eq!(decoded.title, request.title);
    assert_eq!(decoded.body, request.body);
    assert_eq!(decoded.severity, request.severity);
    assert_eq!(decoded.ttl_ms, request.ttl_ms);
}
