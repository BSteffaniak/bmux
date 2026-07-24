//! Smoke tests for the BPDL-generated cluster plugin contract.

use bmux_cluster_plugin_api::{
    cluster_command, cluster_connection_events, cluster_query,
    cluster_types::{
        ClusterConnectionEvent, ClusterConnectionState, ClusterConsensusRole, ClusterHostState,
        ClusterHostStatus, ClusterIdentity, ClusterJoinResult, ClusterLaunchStatus,
        ClusterLeaveResult, ClusterMember, ClusterMemberList, ClusterMemberState,
        ClusterNodeCapabilities, ClusterPaneMutationResult, ClusterUpResult, EnrollmentTokenResult,
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
    let capabilities = ClusterNodeCapabilities {
        consensus_role: ClusterConsensusRole::Voter,
        worker: true,
        ingress: true,
    };
    let identity = ClusterIdentity {
        cluster_id: Some("cluster:0194f776-7c0d-7000-8000-000000000000".to_string()),
        node_id: format!("node:{}", "0".repeat(64)),
        public_key: "0".repeat(64),
        capabilities: Some(capabilities.clone()),
    };
    let identity_json = serde_json::to_string(&identity).expect("identity should serialize");
    assert_eq!(
        serde_json::from_str::<ClusterIdentity>(&identity_json)
            .expect("identity should deserialize"),
        identity
    );

    let member = ClusterMember {
        cluster_id: identity.cluster_id.clone().expect("cluster id"),
        node_id: identity.node_id.clone(),
        public_key: identity.public_key.clone(),
        endpoint: Some("node-a".to_string()),
        capabilities,
        joined_at_unix_ms: 1,
        updated_at_unix_ms: 2,
        state: ClusterMemberState::Active,
    };
    let members = ClusterMemberList {
        cluster_id: identity.cluster_id.clone(),
        members: vec![member.clone()],
    };
    let join = ClusterJoinResult {
        identity: identity.clone(),
        member: member.clone(),
        members: vec![member],
    };
    let leave = ClusterLeaveResult {
        leave_id: "leave".to_string(),
        node_id: identity.node_id.clone(),
        left: true,
    };
    let token = EnrollmentTokenResult {
        token: "token".to_string(),
        expires_at_unix_ms: 42,
    };
    for value in [
        serde_json::to_value(members).expect("members serialize"),
        serde_json::to_value(join).expect("join serialize"),
        serde_json::to_value(leave).expect("leave serialize"),
        serde_json::to_value(token).expect("token serialize"),
    ] {
        assert!(value.is_object());
    }

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
    assert_eq!(cluster_query::INTERFACE_ID, "cluster-query/v1");
    assert_eq!(cluster_command::INTERFACE_ID, "cluster-command/v1");
    assert_eq!(
        cluster_connection_events::INTERFACE_ID,
        "cluster-connection-events/v1"
    );
    assert_eq!(cluster_query::OP_LIST_CLUSTERS, "list_clusters");
    assert_eq!(cluster_query::OP_MEMBERS, "members");
    assert_eq!(cluster_command::OP_INIT, "init");
    assert_eq!(
        cluster_command::OP_ENROLLMENT_TOKEN_CREATE,
        "enrollment_token_create"
    );
    assert_eq!(cluster_command::OP_JOIN, "join");
    assert_eq!(cluster_command::OP_LEAVE_PREPARE, "leave_prepare");
    assert_eq!(cluster_command::OP_REDEEM_ENROLLMENT, "redeem_enrollment");
    assert_eq!(cluster_command::OP_LEAVE, "leave");
    assert_eq!(cluster_command::OP_ACCEPT_LEAVE, "accept_leave");
    assert_eq!(cluster_command::OP_PANE_MOVE, "pane_move");
    assert_eq!(cluster_connection_events::OP_LIST, "list");
}

#[test]
fn generated_contract_declares_all_services() {
    let services = bmux_cluster_plugin_api::service_declarations()
        .expect("cluster service declarations should be valid");
    assert_eq!(services.len(), 3);
}
