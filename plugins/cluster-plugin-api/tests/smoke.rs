//! Smoke tests for the BPDL-generated cluster plugin contract.

use bmux_cluster_plugin_api::{
    cluster_command, cluster_connection_events, cluster_control_command,
    cluster_control_command_v2, cluster_control_state, cluster_control_state_v2, cluster_peer_auth,
    cluster_query,
    cluster_types::{
        ClusterConnectionEvent, ClusterConnectionState, ClusterConsensusRole, ClusterHostState,
        ClusterHostStatus, ClusterIdentity, ClusterJoinResult, ClusterLaunchStatus,
        ClusterLeaveResult, ClusterMember, ClusterMemberList, ClusterMemberState,
        ClusterNegotiatedProtocol, ClusterNodeCapabilities, ClusterPaneMutationResult,
        ClusterProtocolOffer, ClusterUpResult, CommandId, ControlCommand, ControlCommandError,
        ControlCommandRequest, ControlCommandResult, ControlReadConsistency, ControlResourceKind,
        ControlResponse, ControlServiceError, ControlStateView, ControlWorkflowStatus,
        EnrollmentTokenResult, ExecutionAssignment, ExecutionId, LogicalPaneId, LogicalPaneRecord,
        LogicalTabId, LogicalTabRecord, ObservedExecution, PaneAvailability, PaneRestartPolicy,
        PeerAuthChallenge, PeerAuthProof, PlacementIntent, PromotionId, RaftRpcRequest,
        WorkerAdoptionSpec, WorkerAuthority, WorkerExecution, WorkerExecutionList,
        WorkerExecutionState, WorkerLaunchResult, WorkerLaunchSpec, WorkerMutationAck,
        WorkerOperationClass, WorkerQueryResult, WorkerRoute, WorkerServiceError, WorkerSignal,
        WorkspaceId, WorkspaceRecord,
    },
    cluster_worker_command, cluster_worker_state, cluster_worker_stream,
};

#[test]
fn owned_vote_contract_round_trips() {
    use bmux_cluster_plugin_api::{
        cluster_consensus_rpc, cluster_consensus_vote_types::VoteResponse,
    };
    assert_eq!(
        cluster_consensus_rpc::INTERFACE_ID,
        "cluster-consensus-rpc/v1"
    );
    let response = VoteResponse {
        request_id: uuid::Uuid::new_v4(),
        source: vec![1; 32],
        term: 9,
        granted: false,
        signature: vec![2; 64],
    };
    let decoded: VoteResponse =
        bmux_codec::from_bytes(&bmux_codec::to_vec(&response).unwrap()).unwrap();
    assert_eq!(decoded.request_id, response.request_id);
    assert_eq!(decoded.source, response.source);
    assert_eq!(decoded.term, response.term);
    assert_eq!(decoded.granted, response.granted);
    assert_eq!(decoded.signature, response.signature);
}

#[test]
fn owned_append_v2_contract_is_explicit_and_round_trips() {
    use bmux_cluster_plugin_api::{
        cluster_consensus_rpc_types::AppendOutcome as V1Outcome,
        cluster_consensus_rpc_v2,
        cluster_consensus_rpc_v2_types::{AppendOutcome, AppendResponse},
    };
    assert_eq!(
        cluster_consensus_rpc_v2::INTERFACE_ID,
        "cluster-consensus-rpc/v2"
    );
    assert_eq!(cluster_consensus_rpc_v2::OP_APPEND, "append");
    let response = AppendResponse {
        request_id: uuid::Uuid::from_u128(19),
        source: vec![1; 32],
        term: 10,
        outcome: AppendOutcome::StaleTerm,
        signature: vec![2; 64],
    };
    let bytes = bmux_plugin_sdk::encode_service_message(&response).unwrap();
    let decoded: AppendResponse = bmux_plugin_sdk::decode_service_message(&bytes).unwrap();
    assert_eq!(decoded, response);
    for value in [
        V1Outcome::Durable { matched: None },
        V1Outcome::Conflict { next_index: 3 },
    ] {
        let accepted = AppendResponse {
            outcome: AppendOutcome::Accepted { value },
            ..response.clone()
        };
        let decoded: AppendResponse = bmux_plugin_sdk::decode_service_message(
            &bmux_plugin_sdk::encode_service_message(&accepted).unwrap(),
        )
        .unwrap();
        assert_eq!(decoded, accepted);
    }
}

#[test]
fn generated_control_services_expose_consistency_and_structured_errors() {
    let view = ControlStateView {
        membership_epoch: None,
        schema_version: 1,
        cluster_id: "cluster:test".to_string(),
        revision: 7,
        members: Vec::new(),
        workspaces: Vec::new(),
        tabs: Vec::new(),
        panes: Vec::new(),
        pending_workflows: Vec::new(),
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
    assert_eq!(
        bmux_cluster_plugin_api::cluster_attach_state::INTERFACE_ID,
        "cluster-attach-state/v2"
    );
}

#[test]
fn peer_rpc_request_is_generated_and_identity_bound() {
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
    assert_eq!(
        bmux_cluster_plugin_api::cluster_peer_read::INTERFACE_ID,
        "cluster-peer-read/v1"
    );
}

#[test]
fn control_state_contract_uses_typed_ids_and_tagged_commands() {
    let workspace_id = WorkspaceId {
        value: uuid::Uuid::new_v4(),
    };
    let tab_id = LogicalTabId {
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
    let tab = LogicalTabRecord {
        tab_id: tab_id.clone(),
        workspace_id: workspace_id.clone(),
        name: Some("main".to_string()),
        layout_schema_version: 1,
        layout: Vec::new(),
        revision: 1,
    };
    let pane = LogicalPaneRecord {
        pane_id: pane_id.clone(),
        workspace_id: workspace_id.clone(),
        tab_id,
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
        ControlCommandRequest::PutTab {
            tab,
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
            launch_spec: None,
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
        serde_json::to_value(WorkerRoute {
            execution_id: execution_id.clone(),
            generation: authority.generation,
            node_id: "worker".to_string(),
            endpoint: "local".to_string(),
            authority: authority.clone(),
        })
        .unwrap(),
        serde_json::to_value(ObservedExecution {
            execution_id: execution_id.clone(),
            generation: authority.generation,
            session_id: uuid::Uuid::nil(),
            pane_id: uuid::Uuid::nil(),
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
        membership_epoch: None,
        cluster_id: identity.cluster_id.clone(),
        members: vec![member.clone()],
    };
    let join = ClusterJoinResult {
        voter_authorization: None,
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
    assert_eq!(
        cluster_worker_command::INTERFACE_ID,
        "cluster-worker-command/v2"
    );
    assert_eq!(
        cluster_worker_state::INTERFACE_ID,
        "cluster-worker-state/v2"
    );
    assert_eq!(cluster_worker_command::OP_LAUNCH, "launch");
    assert_eq!(cluster_worker_command::OP_ADOPT, "adopt");
    assert_eq!(cluster_worker_command::OP_INPUT, "input");
    assert_eq!(cluster_worker_command::OP_RESIZE, "resize");
    assert_eq!(cluster_worker_command::OP_SIGNAL, "signal");
    assert_eq!(cluster_worker_command::OP_RESTART, "restart");
    assert_eq!(cluster_worker_command::OP_CLOSE, "close");
    assert_eq!(cluster_worker_state::OP_GET, "get");
    assert_eq!(cluster_worker_stream::OP_OBSERVE, "observe");
    assert_eq!(cluster_worker_state::OP_INVENTORY, "inventory");
    assert_eq!(cluster_control_command::OP_MUTATE, "mutate");
    assert_eq!(cluster_control_command_v2::OP_MUTATE, "mutate");
    assert_eq!(
        cluster_control_command_v2::OP_ACTIVATE_FEATURE,
        "activate-feature"
    );
    assert_eq!(
        cluster_control_state::OP_READ_LINEARIZABLE,
        "read_linearizable"
    );
    assert_eq!(cluster_control_state::OP_READ_STALE, "read_stale");
    assert_eq!(
        cluster_control_state_v2::OP_READ_LINEARIZABLE,
        "read_linearizable"
    );
    assert_eq!(cluster_control_state_v2::OP_READ_STALE, "read_stale");
    assert_eq!(cluster_control_state_v2::OP_FEATURE_FLOOR, "feature-floor");
    assert_eq!(
        bmux_cluster_plugin_api::cluster_consensus_rpc::OP_VOTE,
        "vote"
    );
    assert_eq!(
        bmux_cluster_plugin_api::cluster_consensus_rpc::OP_APPEND,
        "append"
    );
    assert_eq!(
        bmux_cluster_plugin_api::cluster_consensus_rpc_v2::OP_APPEND,
        "append"
    );
    assert_eq!(cluster_connection_events::OP_LIST, "list");
}

#[test]
fn transfer_contract_preserves_full_node_keys() {
    use bmux_cluster_plugin_api::cluster_consensus_transfer_types::{
        EntryPurpose, LogIdentity, TransferHeader, TransferPurpose,
    };
    let sender = (0u8..32).collect::<Vec<_>>();
    let recipient = (32u8..64).collect::<Vec<_>>();
    let header = TransferHeader {
        cluster_id: "cluster".into(),
        sender: sender.clone(),
        recipient,
        leader_term: 7,
        sender_incarnation: uuid::Uuid::from_u128(1),
        sequence: 2,
        purpose: TransferPurpose::Entry {
            value: EntryPurpose {
                identity: LogIdentity {
                    term: 7,
                    leader: sender,
                    index: 3,
                },
                previous: None,
            },
        },
        total_bytes: 12,
        checksum: vec![1; 32],
    };
    let bytes = bmux_plugin_sdk::encode_service_message(&header).unwrap();
    assert_eq!(
        bmux_plugin_sdk::decode_service_message::<TransferHeader>(&bytes).unwrap(),
        header
    );
}

#[test]
fn owned_snapshot_install_contract_is_separate_and_round_trips() {
    use bmux_cluster_plugin_api::cluster_consensus_transfer_types::LogIdentity;
    use bmux_cluster_plugin_api::{
        cluster_consensus_snapshot,
        cluster_consensus_snapshot_types::{
            InstallError, InstallOutcome, InstallRejection, InstallResponse,
        },
    };
    assert_eq!(
        cluster_consensus_snapshot::INTERFACE_ID,
        "cluster-consensus-snapshot/v1"
    );
    assert_eq!(cluster_consensus_snapshot::OP_INSTALL, "install");
    for outcome in [
        InstallOutcome::Installed {
            boundary: LogIdentity {
                term: 7,
                leader: vec![3; 32],
                index: 19,
            },
        },
        InstallOutcome::Rejected {
            reason: InstallRejection::NotSealed,
        },
        InstallOutcome::Rejected {
            reason: InstallRejection::ConflictingReference,
        },
    ] {
        let response = InstallResponse {
            request_id: uuid::Uuid::from_u128(1),
            source: vec![2; 32],
            term: 7,
            outcome,
            signature: vec![5; 64],
        };
        let bytes = bmux_plugin_sdk::encode_service_message(&response).unwrap();
        assert_eq!(
            bmux_plugin_sdk::decode_service_message::<InstallResponse>(&bytes).unwrap(),
            response
        );
    }
    let bytes = bmux_plugin_sdk::encode_service_message(&InstallError::StorageFailure).unwrap();
    assert_eq!(
        bmux_plugin_sdk::decode_service_message::<InstallError>(&bytes).unwrap(),
        InstallError::StorageFailure
    );
    assert_eq!(
        bmux_cluster_plugin_api::cluster_consensus_transfer_types::FinishOutcome::Sealed,
        bmux_plugin_sdk::decode_service_message(
            &bmux_plugin_sdk::encode_service_message(
                &bmux_cluster_plugin_api::cluster_consensus_transfer_types::FinishOutcome::Sealed
            )
            .unwrap()
        )
        .unwrap()
    );
}

#[test]
fn generated_contract_declares_all_services() {
    let services = bmux_cluster_plugin_api::service_declarations()
        .expect("cluster service declarations should be valid");
    assert_eq!(services.len(), 23);
    let service_ids = services
        .iter()
        .map(|service| service.interface_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for interface_id in [
        "cluster-peer-read/v1",
        "cluster-consensus-rpc/v1",
        "cluster-consensus-rpc/v2",
        "cluster-consensus-transfer/v1",
        "cluster-consensus-snapshot/v1",
        "cluster-bounded-control-command/v1",
        "cluster-capability-publication/v1",
        "cluster-protocol-refresh/v1",
        "cluster-principal-bootstrap/v1",
        "cluster-control-command/v1",
        "cluster-control-command/v2",
        "cluster-control-state/v1",
        "cluster-control-state/v2",
    ] {
        assert!(
            service_ids.contains(interface_id),
            "missing generated service declaration {interface_id}"
        );
    }
    // The package world, not a handwritten manifest, declares what is served.
    let package = include_str!("../bpdl/cluster-plugin.bpdl");
    for provided in [
        "provide cluster-worker-command;",
        "provide cluster-attach-state;",
        "provide cluster-attach-command;",
        "provide cluster-worker-state;",
        "provide cluster-worker-stream;",
        "provide cluster-control-watch;",
        "provide cluster-control-command@2;",
        "provide cluster-control-state@2;",
        "provide cluster-bounded-control-command;",
        "provide cluster-consensus-rpc@1;",
        "provide cluster-consensus-rpc@2;",
    ] {
        assert!(package.contains(provided), "world must {provided}");
    }
    // The retired OpenRaft contracts are gone; staged transfer and snapshot
    // installation are served once their providers land.
    for unprovided in [
        "cluster-raft-rpc",
        "cluster-raft-decoder",
        "cluster-consensus-transfer",
        "cluster-consensus-snapshot",
    ] {
        assert!(
            !package.contains(&format!("provide {unprovided}")),
            "world must not provide {unprovided}"
        );
    }
}

#[test]
fn generated_voter_authorization_v2_round_trips_and_legacy_proofs_are_not_promoted() {
    use bmux_cluster_plugin_api::cluster_types::{
        ConsensusVoterChangeAction, ConsensusVoterChangeAuthorization,
    };
    let authorization = ConsensusVoterChangeAuthorization {
        protocol_version: 2,
        cluster_id: "cluster:test".into(),
        membership_epoch: "1:node:test:7".into(),
        current_members_digest: vec![1; 32],
        proposed_members_digest: vec![2; 32],
        expires_at_unix_ms: 123_456,
        action: ConsensusVoterChangeAction::Add,
        target_node_id: "node:joining".into(),
        actor_node_id: "node:issuer".into(),
        signature: vec![3; 64],
    };
    let request =
        bmux_cluster_plugin_api::cluster_command::client::ConsensusReconcileMembersRequest {
            members: Vec::new(),
            authorization: authorization.clone(),
        };
    let encoded = bmux_plugin_sdk::encode_service_message(&request).unwrap();
    let decoded: bmux_cluster_plugin_api::cluster_command::client::ConsensusReconcileMembersRequest =
        bmux_plugin_sdk::decode_service_message(&encoded).unwrap();
    assert_eq!(decoded.authorization, authorization);
    // This is an old peer's record, not a second runtime transport envelope.
    let mut legacy = serde_json::to_value(&authorization).unwrap();
    for field in [
        "protocol_version",
        "membership_epoch",
        "current_members_digest",
        "proposed_members_digest",
        "expires_at_unix_ms",
    ] {
        legacy.as_object_mut().unwrap().remove(field);
    }
    let decoded = serde_json::from_value::<ConsensusVoterChangeAuthorization>(legacy).unwrap();
    // Generated records default absent scalar fields. The authorization verifier
    // explicitly rejects version 0; decoding must never silently create v2.
    assert_eq!(decoded.protocol_version, 0);
    assert!(decoded.membership_epoch.is_empty());
    assert!(decoded.current_members_digest.is_empty());
    assert!(decoded.proposed_members_digest.is_empty());
    assert_eq!(decoded.expires_at_unix_ms, 0);
}
