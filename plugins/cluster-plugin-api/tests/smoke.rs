//! Smoke tests for the BPDL-generated cluster plugin contract.

use bmux_cluster_plugin_api::{
    cluster_command_v1, cluster_connection_events_v1, cluster_query_v1,
    cluster_types::{
        ClusterConnectionEvent, ClusterConnectionState, ClusterHostState, ClusterHostStatus,
        ClusterLaunchStatus, ClusterPaneMutationResult, ClusterUpResult,
    },
};

#[test]
fn cluster_status_types_round_trip() {
    let status = ClusterHostStatus {
        cluster: "prod".to_string(),
        target: "prod-a".to_string(),
        state: ClusterHostState::Ready,
        reason: None,
    };
    let encoded = serde_json::to_string(&status).expect("status should serialize");
    let decoded: ClusterHostStatus =
        serde_json::from_str(&encoded).expect("status should deserialize");
    assert_eq!(decoded, status);
}

#[test]
fn cluster_command_results_round_trip() {
    let launch = ClusterLaunchStatus {
        target: "prod-a".to_string(),
        state: ClusterHostState::Degraded,
        reason: Some("probe failed".to_string()),
        pane_id: None,
    };
    let up = ClusterUpResult {
        session_id: "session".to_string(),
        statuses: vec![launch],
    };
    let mutation = ClusterPaneMutationResult {
        target: "prod-b".to_string(),
        old_pane_id: Some("old".to_string()),
        old_name: Some("prod-a".to_string()),
        new_pane_id: "new".to_string(),
        session_id: "session".to_string(),
    };

    let up_json = serde_json::to_string(&up).expect("up result should serialize");
    let mutation_json = serde_json::to_string(&mutation).expect("mutation result should serialize");
    assert_eq!(
        serde_json::from_str::<ClusterUpResult>(&up_json).expect("up result should deserialize"),
        up
    );
    assert_eq!(
        serde_json::from_str::<ClusterPaneMutationResult>(&mutation_json)
            .expect("mutation result should deserialize"),
        mutation
    );
}

#[test]
fn cluster_connection_event_round_trips() {
    let event = ClusterConnectionEvent {
        ts_unix_ms: 42,
        pane_id: Some("pane".to_string()),
        cluster: Some("prod".to_string()),
        target: Some("prod-a".to_string()),
        source: Some("up".to_string()),
        state: ClusterConnectionState::Connecting,
        message: "connecting".to_string(),
    };
    let encoded = serde_json::to_string(&event).expect("event should serialize");
    let decoded: ClusterConnectionEvent =
        serde_json::from_str(&encoded).expect("event should deserialize");
    assert_eq!(decoded, event);
}

#[test]
fn interface_ids_and_operations_match_schema() {
    assert_eq!(cluster_query_v1::INTERFACE_ID, "cluster-query-v1");
    assert_eq!(cluster_command_v1::INTERFACE_ID, "cluster-command-v1");
    assert_eq!(
        cluster_connection_events_v1::INTERFACE_ID,
        "cluster-connection-events-v1"
    );
    assert_eq!(cluster_query_v1::OP_LIST_CLUSTERS, "list_clusters");
    assert_eq!(cluster_command_v1::OP_PANE_MOVE, "pane_move");
    assert_eq!(cluster_connection_events_v1::OP_LIST_EVENTS, "list_events");
}

#[test]
fn generated_contract_declares_all_services() {
    let services = bmux_cluster_plugin_api::service_declarations()
        .expect("cluster service declarations should be valid");
    assert_eq!(services.len(), 3);
}
