//! Smoke tests for the BPDL-generated cluster plugin contract.

use bmux_cluster_plugin_api::{
    cluster_command, cluster_connection_events, cluster_control_command, cluster_control_state,
    cluster_peer_auth, cluster_query, cluster_raft_rpc,
    cluster_types::{
        ClusterConnectionEvent, ClusterConnectionState, ClusterConsensusRole, ClusterHostState,
        ClusterHostStatus, ClusterIdentity, ClusterJoinResult, ClusterLaunchStatus,
        ClusterLeaveResult, ClusterMember, ClusterMemberList, ClusterMemberState,
        ClusterNegotiatedProtocol, ClusterNodeCapabilities, ClusterPaneMutationResult,
        ClusterProtocolOffer, ClusterUpResult, CommandId, ControlCommand, ControlCommandError,
        ControlCommandRequest, ControlCommandResult, ControlReadConsistency, ControlResourceKind,
        ControlResponse, ControlServiceError, ControlStateView, ControlWorkflowStatus,
        EnrollmentTokenResult, ExecutionAssignment, ExecutionId, LogicalPaneId, LogicalPaneRecord,
        LogicalWindowId, LogicalWindowRecord, PaneAvailability, PaneRestartPolicy,
        PeerAuthChallenge, PeerAuthProof, PlacementIntent, PromotionId, RaftRpcRequest,
        RaftRpcResponse, WorkerAdoptionSpec, WorkerAuthority, WorkerExecution, WorkerExecutionList,
        WorkerExecutionState, WorkerLaunchResult, WorkerLaunchSpec, WorkerMutationAck,
        WorkerOperationClass, WorkerOutput, WorkerQueryResult, WorkerServiceError, WorkerSignal,
        WorkerTerminalSnapshot, WorkspaceId, WorkspaceRecord,
    },
    cluster_worker_command, cluster_worker_state,
};

#[test]
fn generated_control_services_expose_consistency_and_structured_errors() {
    let view = ControlStateView {
        schema_version: 1,
        cluster_id: "cluster:test".to_string(),
        revision: 7,
        members: Vec::new(),
        workspaces: Vec::new(),
        windows: Vec::new(),
        panes: Vec::new(),
        consistency: ControlReadConsistency::Linearizable,
    };
    let encoded = bmux_plugin_sdk::encode_service_message(&view).unwrap();
    assert_eq!(
        bmux_plugin_sdk::decode_service_message::<ControlStateView>(&encoded).unwrap(),
        view
    );
    let error = ControlServiceError::NotLeader {
        leader_node_id: Some("node:leader".to_string()),
        leader_endpoint: Some("tls://leader".to_string()),
    };
    let encoded = bmux_plugin_sdk::encode_service_message(&error).unwrap();
    assert_eq!(
        bmux_plugin_sdk::decode_service_message::<ControlServiceError>(&encoded).unwrap(),
        error
    );
    assert_eq!(
        cluster_control_command::INTERFACE_ID,
        "cluster-control-command/v1"
    );
    assert_eq!(
        cluster_control_state::INTERFACE_ID,
        "cluster-control-state/v1"
    );
}

#[test]
fn raft_rpc_contract_is_generated_and_identity_bound() {
    let challenge = PeerAuthChallenge {
        protocol_version: 1,
        cluster_id: "cluster:0194f776-7c0d-7000-8000-000000000000".to_string(),
        verifier_node_id: "node:target".to_string(),
        verifier_credential_serial: "credential:target".to_string(),
        audience_node_id: "node:claimant".to_string(),
        nonce: "nonce".to_string(),
        issued_at_unix_ms: 1,
        expires_at_unix_ms: 30_001,
        signature: "verifier-signature".to_string(),
    };
    let proof = PeerAuthProof {
        challenge,
        claimant_node_id: "node:claimant".to_string(),
        claimant_credential_serial: "credential:1".to_string(),
        claimant_signature: "claimant-signature".to_string(),
    };
    let request = RaftRpcRequest {
        target_node_id: "node:target".to_string(),
        proof,
        payload: vec![1, 2, 3],
        signature: vec![4, 5, 6],
    };
    let encoded = bmux_plugin_sdk::encode_service_message(&request).unwrap();
    assert_eq!(
        bmux_plugin_sdk::decode_service_message::<RaftRpcRequest>(&encoded).unwrap(),
        request
    );
    let response = RaftRpcResponse {
        source_node_id: "node:target".to_string(),
        payload: vec![4, 5],
        error: None,
    };
    let encoded = bmux_plugin_sdk::encode_service_message(&response).unwrap();
    assert_eq!(
        bmux_plugin_sdk::decode_service_message::<RaftRpcResponse>(&encoded).unwrap(),
        response
    );
    assert_eq!(cluster_raft_rpc::INTERFACE_ID, "cluster-raft-rpc/v1");
}

#[test]
fn control_state_contract_uses_typed_ids_and_tagged_commands() {
    let workspace_id = WorkspaceId {
        value: uuid::Uuid::new_v4(),
    };
    let window_id = LogicalWindowId {
        value: uuid::Uuid::new_v4(),
    };
    let pane_id = LogicalPaneId {
        value: uuid::Uuid::new_v4(),
    };
    let command_id = CommandId {
        value: uuid::Uuid::new_v4(),
    };
    let assignment = ExecutionAssignment {
        node_id: "node:worker".to_string(),
        generation: 1,
        execution_id: ExecutionId {
            value: uuid::Uuid::new_v4(),
        },
    };
    let workspace = WorkspaceRecord {
        workspace_id: workspace_id.clone(),
        name: Some("ops".to_string()),
        revision: 1,
    };
    let window = LogicalWindowRecord {
        window_id: window_id.clone(),
        workspace_id: workspace_id.clone(),
        name: Some("main".to_string()),
        layout_schema_version: 1,
        layout: Vec::new(),
        revision: 1,
    };
    let pane = LogicalPaneRecord {
        pane_id: pane_id.clone(),
        workspace_id: workspace_id.clone(),
        window_id,
        name: Some("shell".to_string()),
        restart_policy: PaneRestartPolicy::Manual,
        placement: PlacementIntent {
            explicit_node_id: Some("node:worker".to_string()),
            required_labels: Vec::new(),
            preferred_labels: Vec::new(),
        },
        availability: PaneAvailability::Ready,
        availability_reason: None,
        execution: Some(assignment.clone()),
        revision: 1,
    };
    let commands = [
        ControlCommandRequest::CreateWorkspace {
            workspace_id: workspace.workspace_id,
            name: workspace.name,
        },
        ControlCommandRequest::PutWindow {
            window,
            expected_workspace_revision: 1,
        },
        ControlCommandRequest::PutPane {
            pane,
            expected_workspace_revision: 2,
        },
        ControlCommandRequest::AssignExecution {
            pane_id,
            expected_revision: 1,
            expected_generation: 0,
            assignment,
        },
    ];
    for request in commands {
        let command = ControlCommand {
            schema_version: 1,
            principal_id: "principal:test".to_string(),
            command_id: command_id.clone(),
            issued_at_unix_ms: 1,
            request,
        };
        let encoded = bmux_plugin_sdk::encode_service_message(&command)
            .expect("control command should encode");
        let decoded: ControlCommand = bmux_plugin_sdk::decode_service_message(&encoded)
            .expect("control command should decode");
        assert_eq!(decoded, command);
    }

    let response = ControlResponse {
        schema_version: 1,
        command_id,
        control_revision: 4,
        workflow_status: ControlWorkflowStatus::Complete,
        result: ControlCommandResult::Rejected {
            error: ControlCommandError::NotFound {
                resource: ControlResourceKind::Pane,
                id: "pane:missing".to_string(),
            },
        },
    };
    let encoded =
        bmux_plugin_sdk::encode_service_message(&response).expect("control response should encode");
    let decoded: ControlResponse =
        bmux_plugin_sdk::decode_service_message(&encoded).expect("control response should decode");
    assert_eq!(decoded, response);

    let promotion = PromotionId {
        value: uuid::Uuid::new_v4(),
    };
    assert_eq!(
        serde_json::from_str::<PromotionId>(&serde_json::to_string(&promotion).unwrap()).unwrap(),
        promotion
    );
}

#[test]
fn generated_worker_contract_carries_execution_identity_and_fencing() {
    let execution_id = ExecutionId {
        value: uuid::Uuid::new_v4(),
    };
    let authority = WorkerAuthority {
        cluster_id: "cluster:test".to_string(),
        workspace_id: WorkspaceId {
            value: uuid::Uuid::new_v4(),
        },
        pane_id: LogicalPaneId {
            value: uuid::Uuid::new_v4(),
        },
        execution_id: execution_id.clone(),
        generation: 7,
        control_term: 11,
        lease_sequence: 3,
        operation_class: WorkerOperationClass::Interactive,
        principal_id: "principal:test".to_string(),
        issuer_node_id: "node:issuer".to_string(),
        audience_node_id: "node:worker".to_string(),
        lease_id: uuid::Uuid::new_v4(),
        lease_issued_at_unix_ms: 21,
        lease_duration_ms: 2_000,
        lease_signature: vec![1, 2, 3],
    };
    let execution = WorkerExecution {
        authority: authority.clone(),
        local_session_id: uuid::Uuid::new_v4(),
        local_pane_id: uuid::Uuid::new_v4(),
        state: WorkerExecutionState::Ready,
        exit_code: None,
        output_start: 5,
        output_end: 9,
    };
    let values = [
        serde_json::to_value(WorkerLaunchSpec {
            program: Some("sh".to_string()),
            args: vec!["-l".to_string()],
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
        })
        .unwrap(),
        serde_json::to_value(WorkerAdoptionSpec {
            local_session_id: execution.local_session_id,
            local_pane_id: execution.local_pane_id,
        })
        .unwrap(),
        serde_json::to_value(WorkerLaunchResult::Ready {
            execution: execution.clone(),
        })
        .unwrap(),
        serde_json::to_value(WorkerQueryResult::Found {
            execution: execution.clone(),
        })
        .unwrap(),
        serde_json::to_value(WorkerExecutionList {
            executions: vec![execution],
        })
        .unwrap(),
        serde_json::to_value(WorkerMutationAck {
            execution_id: execution_id.clone(),
            generation: authority.generation,
            applied: true,
        })
        .unwrap(),
        serde_json::to_value(WorkerOutput {
            execution_id: execution_id.clone(),
            generation: authority.generation,
            requested_cursor: 5,
            retained_start: 5,
            next_cursor: 9,
            gap: false,
            output_still_pending: false,
            data: b"data".to_vec(),
        })
        .unwrap(),
        serde_json::to_value(WorkerTerminalSnapshot {
            execution_id: execution_id.clone(),
            generation: authority.generation,
            cursor: 9,
            encoded: vec![1, 2, 3],
        })
        .unwrap(),
        serde_json::to_value(WorkerServiceError::StaleGeneration {
            expected: 8,
            received: 7,
        })
        .unwrap(),
        serde_json::to_value(WorkerSignal::Interrupt).unwrap(),
    ];
    assert!(values.iter().all(|value| !value.is_null()));
}

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
    let protocol = ClusterProtocolOffer {
        wire_epoch: 3,
        peer_revision_min: 1,
        peer_revision_max: 1,
        schema_version_min: 1,
        schema_version_max: 1,
        plugin_version: "1.0.0".to_string(),
        features: vec![
            "membership-credential-v1".to_string(),
            "node-possession-proof-v1".to_string(),
            "single-use-enrollment-v1".to_string(),
        ],
    };
    let negotiated = ClusterNegotiatedProtocol {
        wire_epoch: 3,
        peer_revision: 1,
        schema_version: 1,
        local_plugin_version: "1.0.0".to_string(),
        remote_plugin_version: "1.0.0".to_string(),
        features: protocol.features.clone(),
    };
    let identity = ClusterIdentity {
        cluster_id: Some("cluster:0194f776-7c0d-7000-8000-000000000000".to_string()),
        node_id: format!("node:{}", "0".repeat(64)),
        public_key: "0".repeat(64),
        capabilities: Some(capabilities.clone()),
        protocol,
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
        credential_serial: "credential-1".to_string(),
        credential_issuer_node_id: identity.node_id.clone(),
        credential_issuer_public_key: identity.public_key.clone(),
        credential_issued_at_unix_ms: 1,
        credential_expires_at_unix_ms: 3,
        credential_signature: "00".repeat(64),
        negotiated_protocol: negotiated,
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
    assert_eq!(cluster_peer_auth::INTERFACE_ID, "cluster-peer-auth/v1");
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
    assert_eq!(cluster_peer_auth::OP_CHALLENGE, "challenge");
    assert_eq!(cluster_peer_auth::OP_PROVE, "prove");
    assert_eq!(cluster_peer_auth::OP_AUTHENTICATE, "authenticate");
    assert_eq!(cluster_raft_rpc::INTERFACE_ID, "cluster-raft-rpc/v1");
    assert_eq!(
        cluster_worker_command::INTERFACE_ID,
        "cluster-worker-command/v1"
    );
    assert_eq!(
        cluster_worker_state::INTERFACE_ID,
        "cluster-worker-state/v1"
    );
    assert_eq!(cluster_worker_command::OP_LAUNCH, "launch");
    assert_eq!(cluster_worker_command::OP_ADOPT, "adopt");
    assert_eq!(cluster_worker_command::OP_INPUT, "input");
    assert_eq!(cluster_worker_command::OP_RESIZE, "resize");
    assert_eq!(cluster_worker_command::OP_SIGNAL, "signal");
    assert_eq!(cluster_worker_command::OP_RESTART, "restart");
    assert_eq!(cluster_worker_command::OP_CLOSE, "close");
    assert_eq!(cluster_worker_state::OP_GET, "get");
    assert_eq!(cluster_worker_state::OP_OUTPUT, "output");
    assert_eq!(cluster_worker_state::OP_SNAPSHOT, "snapshot");
    assert_eq!(cluster_worker_state::OP_INVENTORY, "inventory");
    assert_eq!(cluster_control_command::OP_MUTATE, "mutate");
    assert_eq!(
        cluster_control_state::OP_READ_LINEARIZABLE,
        "read_linearizable"
    );
    assert_eq!(cluster_control_state::OP_READ_STALE, "read_stale");
    assert_eq!(cluster_raft_rpc::OP_APPEND_ENTRIES, "append_entries");
    assert_eq!(cluster_raft_rpc::OP_INSTALL_SNAPSHOT, "install_snapshot");
    assert_eq!(cluster_raft_rpc::OP_VOTE, "vote");
    assert_eq!(cluster_connection_events::OP_LIST, "list");
}

#[test]
fn generated_contract_declares_all_services() {
    let services = bmux_cluster_plugin_api::service_declarations()
        .expect("cluster service declarations should be valid");
    assert_eq!(services.len(), 9);
    let manifest = include_str!("../../cluster-plugin/plugin.toml");
    assert!(manifest.contains("cluster-worker-command/v1"));
    assert!(manifest.contains("cluster-worker-state/v1"));
}
