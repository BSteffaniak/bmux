#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::wildcard_imports)] // Focused private modules expose a crate-internal domain facade.

pub(crate) mod commands;
pub mod consensus_membership;
pub mod consensus_network;
pub mod consensus_runtime;
pub mod consensus_storage;
pub mod control_codec;
pub mod control_state;
pub(crate) mod endpoint;
pub(crate) mod events;
mod gateway;
pub(crate) mod membership;
pub(crate) mod pane;
pub mod placement;
pub(crate) mod storage;
pub mod worker_pane_runtime;
pub mod worker_runtime;
pub(crate) mod workspace;

pub(crate) use commands::*;
pub(crate) use events::*;
pub(crate) use membership::*;
pub(crate) use pane::*;
pub(crate) use storage::*;
pub(crate) use workspace::*;

pub(crate) use bmux_cluster_plugin_api::{
    cluster_command::client::{
        AcceptLeaveRequest as ClusterCommandAcceptLeaveRequest,
        CredentialRotateAcceptRequest as ClusterCommandCredentialRotateAcceptRequest,
        CredentialRotateCommitRequest as ClusterCommandCredentialRotateCommitRequest,
        EnrollmentRevokeRequest as ClusterCommandEnrollmentRevokeRequest,
        EnrollmentTokenCreateRequest as ClusterCommandEnrollmentTokenCreateRequest,
        JoinRequest as ClusterCommandJoinRequest,
        MemberRevokeRequest as ClusterCommandMemberRevokeRequest,
        PaneMoveRequest as ClusterCommandPaneMoveRequest,
        PaneNewRequest as ClusterCommandPaneNewRequest,
        PaneRetryRequest as ClusterCommandPaneRetryRequest,
        RedeemEnrollmentRequest as ClusterCommandRedeemEnrollmentRequest,
        UpRequest as ClusterCommandUpRequest,
    },
    cluster_peer_auth::client::{
        AuthenticateRequest as ClusterPeerAuthenticateRequest,
        ChallengeRequest as ClusterPeerChallengeRequest, ProveRequest as ClusterPeerProveRequest,
    },
    cluster_query::client::StatusRequest as ClusterQueryStatusRequest,
    cluster_types::{
        AuthenticatedPeer, ClusterConnectionEvent,
        ClusterConnectionEventList as ClusterConnectionEventsListResponse, ClusterConnectionState,
        ClusterConsensusRole, ClusterHostState, ClusterHostStatus,
        ClusterIdentity as ClusterIdentityResponse, ClusterJoinResult, ClusterLaunchStatus,
        ClusterLeaveRequest, ClusterLeaveResult,
        ClusterListResult as ClusterQueryListClustersResponse, ClusterMember, ClusterMemberList,
        ClusterMemberState, ClusterNegotiatedProtocol, ClusterNodeCapabilities,
        ClusterPaneMutationResult as ClusterCommandPaneMutationResponse, ClusterProtocolOffer,
        ClusterStatusResult as ClusterQueryStatusResponse,
        ClusterUpResult as ClusterCommandUpResponse, CredentialRotationRequest, EnrollmentList,
        EnrollmentRevocationResult, EnrollmentState, EnrollmentStatus, EnrollmentTokenResult,
        MemberCredentialResult, MemberLivenessState, MemberStatus, MembershipStatus,
        PeerAuthChallenge, PeerAuthProof,
    },
};
pub(crate) use bmux_config::BmuxConfig;
pub(crate) use bmux_plugin::prompt;
pub(crate) use bmux_plugin_sdk::prelude::*;
pub(crate) use bmux_plugin_sdk::{
    CoreCliCommandRequest, NativeCommandContext, StorageGetRequest, StorageSetRequest,
};
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use std::collections::{BTreeMap, BTreeSet};
pub(crate) use std::path::PathBuf;
pub(crate) use std::sync::Arc;
pub(crate) use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const CLUSTER_PANE_BINDING_PREFIX: &str = "cluster.pane.";
pub(crate) const CLUSTER_CONNECTION_EVENTS_MAX: usize = 256;

#[derive(Default)]
pub struct ClusterPlugin {
    node_identity: Option<NodeIdentity>,
    cluster_id: Option<ClusterId>,
}

impl RustPlugin for ClusterPlugin {
    type Contract = bmux_cluster_plugin_api::Contract;

    fn activate(&mut self, context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        let node_identity = load_or_create_node_identity(&context).map_err(|error| {
            PluginCommandError::failed(format!(
                "failed initializing cluster node identity: {error}"
            ))
        })?;
        let cluster_id = load_cluster_id(&context).map_err(|error| {
            PluginCommandError::failed(format!("failed loading cluster identity: {error}"))
        })?;
        tracing::info!(
            node_id = %node_identity.node_id(),
            cluster_id = cluster_id.as_ref().map(ToString::to_string),
            "cluster identity loaded"
        );
        self.node_identity = Some(node_identity);
        self.cluster_id = cluster_id;
        let configured_endpoint = configured_consensus_endpoint(context.settings.as_ref())
            .map_err(PluginCommandError::failed)?;
        configure_local_consensus_endpoint(&context, configured_endpoint.as_deref()).map_err(
            |error| {
                PluginCommandError::failed(format!(
                    "failed configuring local consensus endpoint: {error}"
                ))
            },
        )?;
        Ok(EXIT_OK)
    }

    fn activate_with_async(
        &mut self,
        context: NativeLifecycleContext,
        async_handle: bmux_plugin_sdk::HostAsyncHandle,
    ) -> Result<i32, PluginCommandError> {
        let result = self.activate(context.clone())?;
        let caller = Arc::new(bmux_plugin::TypedServiceCaller::from_lifecycle_context(
            &context,
        ));
        if let Some((cluster_id, identity, member, single_member)) =
            local_consensus_member(caller.as_ref()).map_err(PluginCommandError::failed)?
        {
            let endpoint = member.endpoint.ok_or_else(|| {
                PluginCommandError::failed(format!(
                    "active consensus voter {} has no advertised endpoint; set plugins.settings.\"bmux.cluster\".consensus_endpoint or advertise it during enrollment",
                    identity.node_id()
                ))
            })?;
            let node_id = *identity.node_id();
            let cluster_id = cluster_id.to_string();
            let state_dir = PathBuf::from(&context.connection.state_dir);
            let nodes = consensus_network::global_consensus_nodes();
            async_handle.spawn_with_name("bmux-cluster-consensus", async move {
                match consensus_runtime::ConsensusNode::start_endpoint(
                    &state_dir,
                    &cluster_id,
                    identity,
                    caller,
                )
                .await
                {
                    Ok(node) => {
                        if let Err(error) = nodes.insert(node_id, node.clone()) {
                            tracing::error!(%error, %node_id, "failed registering consensus node");
                            let _ = node.shutdown().await;
                            return;
                        }
                        if single_member {
                            match node.raft().is_initialized().await {
                                Ok(true) => {}
                                Ok(false) => {
                                    if let Err(error) =
                                        node.initialize_single(node_id, endpoint.clone()).await
                                    {
                                        tracing::error!(%error, %node_id, %endpoint, "failed initializing single-voter consensus cluster");
                                        let _ = nodes.remove(node_id);
                                        let _ = node.shutdown().await;
                                        return;
                                    }
                                }
                                Err(error) => {
                                    tracing::error!(%error, %node_id, "failed reading consensus initialization state");
                                    let _ = nodes.remove(node_id);
                                    let _ = node.shutdown().await;
                                    return;
                                }
                            }
                        }
                        tracing::info!(%node_id, %endpoint, "cluster consensus node started");
                    }
                    Err(error) => {
                        tracing::error!(%error, %node_id, "failed starting cluster consensus node");
                    }
                }
            });
        }
        Ok(result)
    }

    fn deactivate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        if let Some(identity) = self.node_identity.as_ref() {
            let node_id = *identity.node_id();
            if let Ok(Some(node)) = consensus_network::global_consensus_nodes().remove(node_id) {
                let handle = tokio::runtime::Handle::try_current().map_err(|error| {
                    PluginCommandError::failed(format!(
                        "consensus shutdown requires the host runtime: {error}"
                    ))
                })?;
                tokio::task::block_in_place(|| handle.block_on(node.shutdown())).map_err(
                    |error| {
                        PluginCommandError::failed(format!("consensus shutdown failed: {error}"))
                    },
                )?;
            }
        }
        Ok(EXIT_OK)
    }

    fn register_typed_services(
        &self,
        context: bmux_plugin_sdk::TypedServiceRegistrationContext<'_>,
        registry: &mut bmux_plugin_sdk::TypedServiceRegistry,
    ) {
        let caller = Arc::new(bmux_plugin::TypedServiceCaller::from_registration_context(
            &context,
        ));
        let Ok(identity) = load_or_create_node_identity(caller.as_ref()) else {
            return;
        };
        let nodes = consensus_network::global_consensus_nodes();
        let raft_handle: Arc<
            dyn bmux_cluster_plugin_api::cluster_raft_rpc::ClusterRaftRpcService + Send + Sync,
        > = Arc::new(consensus_network::RaftRpcServiceHandle::new(
            caller.clone(),
            *identity.node_id(),
            nodes.clone(),
        ));
        let control = Arc::new(consensus_network::ControlServiceHandle::new(
            caller.clone(),
            *identity.node_id(),
            nodes,
        ));
        let control_commands: Arc<
            dyn bmux_cluster_plugin_api::cluster_control_command::ClusterControlCommandService
                + Send
                + Sync,
        > = control.clone();
        let control_state: Arc<
            dyn bmux_cluster_plugin_api::cluster_control_state::ClusterControlStateService
                + Send
                + Sync,
        > = control;
        let worker = Arc::new(worker_runtime::WorkerServiceHandle::new(
            worker_pane_runtime::local_worker_registry(
                caller.clone(),
                identity.node_id().to_string(),
                worker_runtime::NodeSignatureLeaseVerifier::new(caller),
            ),
        ));
        let worker_commands: Arc<
            dyn bmux_cluster_plugin_api::cluster_worker_command::ClusterWorkerCommandService
                + Send
                + Sync,
        > = worker.clone();
        let worker_state: Arc<
            dyn bmux_cluster_plugin_api::cluster_worker_state::ClusterWorkerStateService
                + Send
                + Sync,
        > = worker;
        let _ = bmux_cluster_plugin_api::cluster_raft_rpc::register_provider(registry, raft_handle);
        let _ = bmux_cluster_plugin_api::cluster_control_command::register_provider(
            registry,
            control_commands,
        );
        let _ = bmux_cluster_plugin_api::cluster_control_state::register_provider(
            registry,
            control_state,
        );
        let _ = bmux_cluster_plugin_api::cluster_worker_command::register_provider(
            registry,
            worker_commands,
        );
        let _ = bmux_cluster_plugin_api::cluster_worker_state::register_provider(
            registry,
            worker_state,
        );
    }

    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        let gateway_result = if context.command.starts_with("cluster-") {
            let handle = tokio::runtime::Handle::try_current().map_err(|error| {
                PluginCommandError::failed(format!(
                    "cluster gateway dispatch requires the host tokio runtime: {error}"
                ))
            })?;
            tokio::task::block_in_place(|| {
                handle.block_on(gateway::run_command(
                    &context,
                    "bmux.cluster",
                    &context.command,
                    &context.arguments,
                ))
            })
            .map_err(|error| PluginCommandError::failed(error.to_string()))?
        } else {
            None
        };
        if let Some(code) = gateway_result {
            return Ok(i32::from(code));
        }

        bmux_plugin_sdk::route_command!(context, {
            "cluster-init" => run_cluster_init(&context).map_err(PluginCommandError::from),
            "cluster-enrollment-token-create" => run_cluster_enrollment_token_create(&context).map_err(PluginCommandError::from),
            "cluster-enrollment-list" => run_cluster_enrollment_list(&context).map_err(PluginCommandError::from),
            "cluster-enrollment-revoke" => run_cluster_enrollment_revoke(&context).map_err(PluginCommandError::from),
            "cluster-credential-rotate" => run_cluster_credential_rotate(&context).map_err(PluginCommandError::from),
            "cluster-member-revoke" => run_cluster_member_revoke(&context).map_err(PluginCommandError::from),
            "cluster-join" => run_cluster_join(&context).map_err(PluginCommandError::from),
            "cluster-leave" => run_cluster_leave(&context).map_err(PluginCommandError::from),
            "cluster-members" => run_cluster_members(&context).map_err(PluginCommandError::from),
            "cluster-hosts" => run_cluster_hosts(&context).map_err(PluginCommandError::from),
            "cluster-status" => run_cluster_status(&context).map_err(PluginCommandError::from),
            "cluster-doctor" => run_cluster_doctor(&context).map_err(PluginCommandError::from),
            "cluster-events" => run_cluster_events(&context).map_err(PluginCommandError::from),
            "cluster-up" => run_cluster_up(&context).map_err(PluginCommandError::from),
            "cluster-pane-new" => run_cluster_pane_new(&context).map_err(PluginCommandError::from),
            "cluster-pane-move" => run_cluster_pane_move(&context).map_err(PluginCommandError::from),
            "cluster-pane-retry" => run_cluster_pane_retry(&context).map_err(PluginCommandError::from)
        })
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        if is_cluster_lifecycle_service(&context) {
            return invoke_cluster_lifecycle_service(&context);
        }
        bmux_plugin_sdk::route_service!(context, {
            "cluster-query/v1", "list_clusters" => |(): (), ctx| {
                let inventory = load_cluster_inventory_for_context(&ctx.connection.config_dir, ctx.settings.clone())
                    .map_err(|error| ServiceResponse::error("list_clusters_failed", error))?;
                Ok(ClusterQueryListClustersResponse {
                    clusters: inventory.clusters,
                })
            },
            "cluster-query/v1", "status" => |req: ClusterQueryStatusRequest, ctx| {
                let inventory = load_cluster_inventory_for_context(&ctx.connection.config_dir, ctx.settings.clone())
                    .map_err(|error| ServiceResponse::error("status_failed", error))?;
                let probe = if req.doctor.unwrap_or(false) {
                    HealthProbe::Doctor
                } else {
                    HealthProbe::Test
                };
                let statuses = collect_statuses_for_selector(ctx, &inventory, req.selector.as_deref(), probe)
                    .map_err(|error| ServiceResponse::error("status_failed", error))?;
                Ok(ClusterQueryStatusResponse { statuses })
            },
            "cluster-command/v1", "up" => |req: ClusterCommandUpRequest, ctx| {
                let inventory = load_cluster_inventory_for_context(&ctx.connection.config_dir, ctx.settings.clone())
                    .map_err(|error| ServiceResponse::error("up_failed", error))?;
                let result = execute_cluster_up(
                    ctx,
                    &inventory,
                    ClusterUpArgs {
                        cluster: req.cluster,
                        hosts: req.hosts,
                        on_failure: RetryFailurePolicy::Continue,
                        retries: 0,
                    },
                )
                .map_err(|error| ServiceResponse::error("up_failed", error))?;
                Ok(ClusterCommandUpResponse {
                    session_id: result.session_id,
                    statuses: result.statuses,
                })
            },
            "cluster-command/v1", "pane_new" => |req: ClusterCommandPaneNewRequest, ctx| {
                let result = execute_cluster_pane_new(
                    ctx,
                    ClusterPaneNewArgs {
                        host: req.host,
                        name: req.name,
                    },
                )
                .map_err(|error| ServiceResponse::error("pane_new_failed", error))?;
                Ok(result)
            },
            "cluster-command/v1", "pane_retry" => |req: ClusterCommandPaneRetryRequest, ctx| {
                let pane = parse_pane_retry_ref(req.pane.unwrap_or_else(|| "active".to_string()));
                let result = execute_cluster_pane_retry(ctx, &ClusterPaneRetryArgs {
                    pane,
                    on_failure: RetryFailurePolicy::Abort,
                    retries: 0,
                })
                    .map_err(|error| ServiceResponse::error("pane_retry_failed", error))?;
                Ok(result)
            },
            "cluster-command/v1", "pane_move" => |req: ClusterCommandPaneMoveRequest, ctx| {
                let pane = parse_pane_retry_ref(req.pane.unwrap_or_else(|| "active".to_string()));
                let result = execute_cluster_pane_move(
                    ctx,
                    ClusterPaneMoveArgs {
                        pane,
                        host: req.host,
                    },
                )
                .map_err(|error| ServiceResponse::error("pane_move_failed", error))?;
                Ok(result)
            },
            "cluster-connection-events/v1", "list" => |(): (), ctx| {
                let events = get_cluster_connection_events(ctx)
                    .map_err(|error| ServiceResponse::error("connection_events_list_failed", error))?;
                Ok(ClusterConnectionEventsListResponse { events })
            },
        })
    }
}

fn is_cluster_lifecycle_service(context: &NativeServiceContext) -> bool {
    matches!(
        (
            context.request.service.interface_id.as_str(),
            context.request.operation.as_str()
        ),
        (
            "cluster-command/v1",
            "join"
                | "leave_prepare"
                | "leave"
                | "accept_leave"
                | "enrollment_token_create"
                | "enrollment_revoke"
                | "credential_rotate_prepare"
                | "credential_rotate_accept"
                | "credential_rotate_commit"
                | "member_revoke"
                | "redeem_enrollment"
                | "init"
        ) | (
            "cluster-query/v1",
            "identity" | "members" | "membership_status" | "enrollments"
        ) | (
            "cluster-peer-auth/v1",
            "challenge" | "prove" | "authenticate"
        )
    )
}

fn reconcile_consensus_members(
    ctx: &NativeServiceContext,
    req: bmux_cluster_plugin_api::cluster_command::client::ConsensusReconcileMembersRequest,
) -> Result<ClusterMemberList, String> {
    let current = list_members(ctx)?;
    let cluster_id = current
        .cluster_id
        .as_deref()
        .ok_or_else(|| "cluster membership is not initialized".to_string())?;
    let target = req
        .members
        .iter()
        .find(|member| {
            member.state == ClusterMemberState::Active
                && member.capabilities.consensus_role == ClusterConsensusRole::Voter
                && !current.members.iter().any(|existing| {
                    existing.node_id == member.node_id
                        && existing.state == ClusterMemberState::Active
                        && existing.capabilities.consensus_role == ClusterConsensusRole::Voter
                })
        })
        .ok_or_else(|| "membership transition does not add a voter".to_string())?;
    consensus_membership::verify_voter_change_authorization(
        &req.authorization,
        cluster_id,
        bmux_cluster_plugin_api::cluster_types::ConsensusVoterChangeAction::Add,
        target.node_id.parse::<NodeId>()?,
        &current.members,
    )?;
    consensus_membership::validate_membership_transition(&current.members, &req.members)?;
    let identity = load_or_create_node_identity(ctx)?;
    let nodes = consensus_network::global_consensus_nodes();
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|error| format!("consensus reconciliation requires the host runtime: {error}"))?;
    tokio::task::block_in_place(|| {
        handle.block_on(consensus_membership::reconcile_members(
            req.members,
            *identity.node_id(),
            &nodes,
        ))
    })?;
    list_members(ctx)
}

fn remove_consensus_voter(
    ctx: &NativeServiceContext,
    req: &bmux_cluster_plugin_api::cluster_command::client::ConsensusRemoveVoterRequest,
) -> Result<ClusterMemberList, String> {
    let identity = load_or_create_node_identity(ctx)?;
    let remove_node_id = req.authorization.target_node_id.parse::<NodeId>()?;
    let membership = list_members(ctx)?;
    let cluster_id = membership
        .cluster_id
        .as_deref()
        .ok_or_else(|| "cluster membership is not initialized".to_string())?;
    consensus_membership::verify_voter_change_authorization(
        &req.authorization,
        cluster_id,
        bmux_cluster_plugin_api::cluster_types::ConsensusVoterChangeAction::Remove,
        remove_node_id,
        &membership.members,
    )?;
    let nodes = consensus_network::global_consensus_nodes();
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|error| format!("consensus voter removal requires the host runtime: {error}"))?;
    tokio::task::block_in_place(|| {
        handle.block_on(consensus_membership::remove_voter(
            membership.members,
            remove_node_id,
            *identity.node_id(),
            &nodes,
        ))
    })?;
    list_members(ctx)
}

#[allow(clippy::too_many_lines)]
fn invoke_cluster_lifecycle_service(context: &NativeServiceContext) -> ServiceResponse {
    bmux_plugin_sdk::route_service!(context, {
        "cluster-command/v1", "join" => |req: ClusterCommandJoinRequest, ctx| {
            adopt_join_result(ctx, &req)
                .map_err(|error| ServiceResponse::error("join_failed", error))
        },
        "cluster-command/v1", "leave_prepare" => |(): (), ctx| {
            prepare_leave(ctx)
                .map_err(|error| ServiceResponse::error("leave_prepare_failed", error))
        },
        "cluster-command/v1", "leave" => |req: bmux_cluster_plugin_api::cluster_command::client::LeaveRequest, ctx| {
            commit_leave(ctx, &req.leave_id)
                .map_err(|error| ServiceResponse::error("leave_failed", error))
        },
        "cluster-command/v1", "accept_leave" => |req: ClusterCommandAcceptLeaveRequest, ctx| {
            accept_leave(ctx, &req)
                .map_err(|error| ServiceResponse::error("accept_leave_failed", error))
        },
        "cluster-command/v1", "enrollment_token_create" => |req: ClusterCommandEnrollmentTokenCreateRequest, ctx| {
            create_enrollment_token(
                ctx,
                &req.request_id,
                &req.endpoint,
                req.ttl_ms,
                req.capabilities,
            )
                .map_err(|error| ServiceResponse::error("enrollment_token_create_failed", error))
        },
        "cluster-command/v1", "enrollment_revoke" => |req: ClusterCommandEnrollmentRevokeRequest, ctx| {
            revoke_enrollment(ctx, &req.enrollment_id)
                .map_err(|error| ServiceResponse::error("enrollment_revoke_failed", error))
        },
        "cluster-command/v1", "credential_rotate_prepare" => |(): (), ctx| {
            prepare_credential_rotation(ctx)
                .map_err(|error| ServiceResponse::error("credential_rotate_prepare_failed", error))
        },
        "cluster-command/v1", "credential_rotate_accept" => |req: ClusterCommandCredentialRotateAcceptRequest, ctx| {
            accept_credential_rotation(ctx, &req.request)
                .map_err(|error| ServiceResponse::error("credential_rotate_accept_failed", error))
        },
        "cluster-command/v1", "credential_rotate_commit" => |req: ClusterCommandCredentialRotateCommitRequest, ctx| {
            commit_credential_rotation(ctx, &req.member)
                .map_err(|error| ServiceResponse::error("credential_rotate_commit_failed", error))
        },
        "cluster-command/v1", "member_revoke" => |req: ClusterCommandMemberRevokeRequest, ctx| {
            revoke_member(ctx, &req.node_id)
                .map_err(|error| ServiceResponse::error("member_revoke_failed", error))
        },
        "cluster-command/v1", "consensus_reconcile_members" => |req: bmux_cluster_plugin_api::cluster_command::client::ConsensusReconcileMembersRequest, ctx| {
            reconcile_consensus_members(ctx, req)
                .map_err(|error| ServiceResponse::error("consensus_reconcile_failed", error))
        },
        "cluster-command/v1", "consensus_remove_voter" => |req: bmux_cluster_plugin_api::cluster_command::client::ConsensusRemoveVoterRequest, ctx| {
            remove_consensus_voter(ctx, &req)
                .map_err(|error| ServiceResponse::error("consensus_remove_voter_failed", error))
        },
        "cluster-command/v1", "redeem_enrollment" => |req: ClusterCommandRedeemEnrollmentRequest, ctx| {
            redeem_enrollment(ctx, &req)
                .map_err(|error| ServiceResponse::error("redeem_enrollment_failed", error))
        },
        "cluster-command/v1", "init" => |req: bmux_cluster_plugin_api::cluster_command::client::InitRequest, ctx| {
            let identity = initialize_cluster(ctx)
                .map_err(|error| ServiceResponse::error("cluster_init_failed", error))?;
            let endpoint = req.endpoint.or(
                configured_consensus_endpoint(ctx.settings.as_ref())
                    .map_err(|error| ServiceResponse::error("cluster_init_failed", error))?
            );
            if let Some(endpoint) = endpoint {
                configure_local_consensus_endpoint(ctx, Some(&endpoint))
                    .map_err(|error| ServiceResponse::error("cluster_init_failed", error))?;
            }
            Ok(identity)
        },
        "cluster-query/v1", "identity" => |(): (), ctx| {
            current_node_identity(ctx)
                .map_err(|error| ServiceResponse::error("identity_failed", error))
        },
        "cluster-query/v1", "members" => |(): (), ctx| {
            list_members(ctx)
                .map_err(|error| ServiceResponse::error("members_failed", error))
        },
        "cluster-query/v1", "membership_status" => |(): (), ctx| {
            membership_status(ctx)
                .map_err(|error| ServiceResponse::error("membership_status_failed", error))
        },
        "cluster-query/v1", "enrollments" => |(): (), ctx| {
            list_enrollments(ctx)
                .map_err(|error| ServiceResponse::error("enrollments_failed", error))
        },
        "cluster-peer-auth/v1", "challenge" => |req: ClusterPeerChallengeRequest, ctx| {
            create_peer_auth_challenge(ctx, &req)
                .map_err(|error| ServiceResponse::error("peer_auth_challenge_failed", error))
        },
        "cluster-peer-auth/v1", "prove" => |req: ClusterPeerProveRequest, ctx| {
            create_peer_auth_proof(ctx, req.challenge)
                .map_err(|error| ServiceResponse::error("peer_auth_proof_failed", error))
        },
        "cluster-peer-auth/v1", "authenticate" => |req: ClusterPeerAuthenticateRequest, ctx| {
            authenticate_peer(ctx, &req)
                .map_err(|error| ServiceResponse::error("peer_authentication_failed", error))
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_sdk::{
        ApiVersion, HostConnectionInfo, HostMetadata, HostScope, ProviderId, RegisteredService,
        ServiceRequest,
    };
    use std::fs;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    #[test]
    fn node_id_is_canonical_copyable_and_cryptographically_valid() {
        let secret = iroh::SecretKey::generate();
        let node_id = NodeId::from_secret_key(&secret);
        let encoded = node_id.to_string();
        assert_eq!(encoded.parse::<NodeId>().unwrap(), node_id);
        assert_eq!(node_id.public_key().unwrap(), secret.public());
        assert_eq!(NodeId::from_bytes(*node_id.as_bytes()), node_id);
        assert_eq!(NodeId::default(), NodeId::from(0));
        assert!("node:not-a-key".parse::<NodeId>().is_err());
    }

    #[derive(Default)]
    struct FakeRuntime {
        inner: Mutex<FakeRuntimeState>,
    }

    #[derive(Default)]
    struct FakeRuntimeState {
        next_id: u128,
        sessions: Vec<SessionSummary>,
        selected_session: Option<Uuid>,
        panes: Vec<PaneSummary>,
        storage: BTreeMap<String, Vec<u8>>,
        health: BTreeMap<String, bool>,
        health_sequences: BTreeMap<String, Vec<bool>>,
        launch_fail_targets: BTreeSet<String>,
        close_fail_panes: BTreeSet<Uuid>,
    }

    impl FakeRuntime {
        fn set_health(&self, target: &str, healthy: bool) {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            guard.health.insert(target.to_string(), healthy);
        }

        fn fail_launch_for(&self, target: &str) {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            guard.launch_fail_targets.insert(target.to_string());
        }

        fn set_health_sequence(&self, target: &str, statuses: Vec<bool>) {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            guard.health_sequences.insert(target.to_string(), statuses);
        }

        fn fail_close_for_pane(&self, pane_id: Uuid) {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            guard.close_fail_panes.insert(pane_id);
        }

        fn storage_value(&self, key: &str) -> Option<Vec<u8>> {
            self.inner
                .lock()
                .expect("runtime lock poisoned")
                .storage
                .get(key)
                .cloned()
        }

        fn set_storage_value(&self, key: &str, value: Vec<u8>) {
            self.inner
                .lock()
                .expect("runtime lock poisoned")
                .storage
                .insert(key.to_string(), value);
        }

        fn add_pane(&self, name: Option<String>, focused: bool) -> Uuid {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            let pane_id = next_test_uuid(&mut guard.next_id);
            let index = u32::try_from(guard.panes.len() + 1).expect("pane index should fit u32");
            if focused {
                for pane in &mut guard.panes {
                    pane.focused = false;
                }
            }
            guard.panes.push(PaneSummary {
                id: pane_id,
                index,
                name,
                focused,
            });
            pane_id
        }
    }

    impl ClusterRuntimeOps for FakeRuntime {
        fn core_cli_command_run_path(
            &self,
            request: &CoreCliCommandRequest,
        ) -> Result<bmux_plugin_sdk::CoreCliCommandResponse, String> {
            let target = request
                .arguments
                .first()
                .ok_or_else(|| "missing target argument".to_string())?;
            let healthy = {
                let mut guard = self.inner.lock().expect("runtime lock poisoned");
                if let Some(sequence) = guard.health_sequences.get_mut(target)
                    && let Some(next) = sequence.first().copied()
                {
                    sequence.remove(0);
                    next
                } else {
                    guard.health.get(target).copied().unwrap_or(false)
                }
            };
            let mut response = bmux_plugin_sdk::CoreCliCommandResponse::new(i32::from(!healthy));
            response.protocol_version = request.protocol_version;
            Ok(response)
        }

        fn session_list(&self) -> Result<SessionListResponse, String> {
            let guard = self.inner.lock().expect("runtime lock poisoned");
            Ok(SessionListResponse {
                sessions: guard.sessions.clone(),
            })
        }

        fn session_create(
            &self,
            request: &SessionCreateRequest,
        ) -> Result<SessionCreateResponse, String> {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            let id = next_test_uuid(&mut guard.next_id);
            guard.sessions.push(SessionSummary {
                id,
                name: request.name.clone(),
                client_count: 1,
            });
            guard.selected_session = Some(id);
            drop(guard);
            Ok(SessionCreateResponse {
                id,
                name: request.name.clone(),
            })
        }

        fn session_select(
            &self,
            request: &SessionSelectRequest,
        ) -> Result<SessionSelectResponse, String> {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            let session_id = match &request.selector {
                SessionSelector::ById(id) => *id,
                SessionSelector::ByName(name) => guard
                    .sessions
                    .iter()
                    .find(|session| session.name.as_deref() == Some(name.as_str()))
                    .map(|session| session.id)
                    .ok_or_else(|| format!("unknown session '{name}'"))?,
            };
            guard.selected_session = Some(session_id);
            Ok(SessionSelectResponse {
                session_id,
                attach_token: next_test_uuid(&mut guard.next_id),
                expires_at_epoch_ms: 0,
            })
        }

        fn pane_list(&self, _request: &PaneListRequest) -> Result<PaneListResponse, String> {
            let guard = self.inner.lock().expect("runtime lock poisoned");
            Ok(PaneListResponse {
                panes: guard.panes.clone(),
            })
        }

        fn pane_launch(&self, request: &PaneLaunchRequest) -> Result<PaneLaunchResponse, String> {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            let target = request
                .command
                .args
                .get(1)
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            if guard.launch_fail_targets.contains(&target) {
                return Err(format!("simulated launch failure for '{target}'"));
            }
            let id = next_test_uuid(&mut guard.next_id);
            for pane in &mut guard.panes {
                pane.focused = false;
            }
            let index = u32::try_from(guard.panes.len() + 1).expect("pane index should fit u32");
            guard.panes.push(PaneSummary {
                id,
                index,
                name: request.name.clone(),
                focused: true,
            });

            let session_id = match request.session.as_ref() {
                Some(SessionSelector::ById(id)) => *id,
                Some(SessionSelector::ByName(name)) => guard
                    .sessions
                    .iter()
                    .find(|session| session.name.as_deref() == Some(name.as_str()))
                    .map(|session| session.id)
                    .ok_or_else(|| format!("unknown session '{name}'"))?,
                None => guard
                    .selected_session
                    .or_else(|| guard.sessions.first().map(|session| session.id))
                    .unwrap_or_else(|| next_test_uuid(&mut guard.next_id)),
            };
            drop(guard);

            Ok(PaneLaunchResponse { id, session_id })
        }

        fn pane_close(&self, request: &PaneCloseRequest) -> Result<PaneCloseResponse, String> {
            let mut guard = self.inner.lock().expect("runtime lock poisoned");
            let target_id = match request.target.as_ref().unwrap_or(&PaneSelector::Active) {
                PaneSelector::ById(id) => *id,
                PaneSelector::ByIndex(index) => guard
                    .panes
                    .iter()
                    .find(|pane| pane.index == *index)
                    .map(|pane| pane.id)
                    .ok_or_else(|| format!("pane index '{index}' not found"))?,
                PaneSelector::Active => guard
                    .panes
                    .iter()
                    .find(|pane| pane.focused)
                    .map(|pane| pane.id)
                    .ok_or_else(|| "no active pane".to_string())?,
            };
            if guard.close_fail_panes.contains(&target_id) {
                return Err(format!("simulated close failure for pane '{target_id}'"));
            }
            guard.panes.retain(|pane| pane.id != target_id);
            if guard.panes.iter().all(|pane| !pane.focused)
                && let Some(first) = guard.panes.first_mut()
            {
                first.focused = true;
            }
            Ok(PaneCloseResponse {
                id: target_id,
                session_id: guard.selected_session.unwrap_or(target_id),
                session_closed: false,
            })
        }

        fn storage_get(
            &self,
            request: &StorageGetRequest,
        ) -> Result<bmux_plugin_sdk::StorageGetResponse, String> {
            let guard = self.inner.lock().expect("runtime lock poisoned");
            Ok(bmux_plugin_sdk::StorageGetResponse {
                value: guard.storage.get(request.key.as_str()).cloned(),
            })
        }

        fn storage_set(&self, request: &StorageSetRequest) -> Result<(), String> {
            self.inner
                .lock()
                .expect("runtime lock poisoned")
                .storage
                .insert(request.key.to_string(), request.value.clone());
            Ok(())
        }
    }

    fn next_test_uuid(counter: &mut u128) -> Uuid {
        *counter += 1;
        Uuid::from_u128(*counter)
    }

    fn complete_test_join(issuer: &FakeRuntime, joiner: &FakeRuntime, request_id: &str) {
        let token =
            create_enrollment_token(issuer, request_id, "issuer", Some(60_000), None).unwrap();
        let signed = decode_and_verify_enrollment_token(&token.token, now_unix_ms()).unwrap();
        let request = enrollment_request(joiner, &signed, token.token.clone(), None);
        let result = redeem_enrollment(issuer, &request).unwrap();
        adopt_join_result(
            joiner,
            &ClusterCommandJoinRequest {
                token: token.token,
                issuer: "issuer".to_string(),
                enrollment_result: result,
            },
        )
        .unwrap();
    }

    fn enrollment_request(
        joiner: &FakeRuntime,
        token: &SignedEnrollmentToken,
        token_text: String,
        endpoint: Option<String>,
    ) -> ClusterCommandRedeemEnrollmentRequest {
        let identity = load_or_create_node_identity(joiner).unwrap();
        let protocol = current_protocol_offer();
        let possession_signature =
            create_enrollment_possession_proof(joiner, token, endpoint.clone(), &protocol).unwrap();
        ClusterCommandRedeemEnrollmentRequest {
            token: token_text,
            node_id: identity.node_id().to_string(),
            public_key: identity.public_key().to_string(),
            endpoint,
            protocol,
            possession_signature,
        }
    }

    struct ServiceTestConfigDir {
        dir: std::path::PathBuf,
    }

    impl ServiceTestConfigDir {
        fn create() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "bmux-cluster-plugin-service-tests-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).expect("service test config dir should be created");
            let config = "[connections.targets.db-a]\ntransport='ssh'\nhost='db-a.example.com'\n[connections.targets.db-b]\ntransport='ssh'\nhost='db-b.example.com'\n";
            fs::write(dir.join("bmux.toml"), config)
                .expect("service test config should be written");
            Self { dir }
        }
    }

    impl Drop for ServiceTestConfigDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn service_test_context_from_payload(
        config_dir: &str,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
        settings: Option<toml::Value>,
    ) -> NativeServiceContext {
        let kind = if interface_id == "cluster-command/v1" {
            ServiceKind::Command
        } else {
            ServiceKind::Query
        };
        let capability = if interface_id == "cluster-command/v1" {
            "bmux.server_clusters.write"
        } else {
            "bmux.server_clusters.read"
        };

        NativeServiceContext {
            plugin_id: "bmux.cluster".to_string(),
            request: ServiceRequest {
                caller_plugin_id: "test.caller".to_string(),
                service: RegisteredService {
                    capability: HostScope::new(capability).expect("capability should parse"),
                    kind,
                    interface_id: interface_id.to_string(),
                    provider: ProviderId::Plugin("bmux.cluster".to_string()),
                },
                operation: operation.to_string(),
                payload,
            },
            required_capabilities: vec![
                "bmux.commands".to_string(),
                "bmux.panes.write".to_string(),
                "bmux.sessions.read".to_string(),
                "bmux.sessions.write".to_string(),
                "bmux.storage".to_string(),
            ],
            provided_capabilities: vec![
                "bmux.server_clusters.read".to_string(),
                "bmux.server_clusters.write".to_string(),
            ],
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: vec!["bmux.cluster".to_string()],
            plugin_search_roots: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: HostConnectionInfo {
                config_dir: config_dir.to_string(),
                config_dir_candidates: vec![config_dir.to_string()],
                runtime_dir: config_dir.to_string(),
                data_dir: config_dir.to_string(),
                state_dir: config_dir.to_string(),
            },
            settings,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            cancellation: bmux_plugin_sdk::CancellationToken::default(),
            host_kernel_bridge: None,
        }
    }

    fn service_test_context<T: Serialize>(
        config_dir: &str,
        interface_id: &str,
        operation: &str,
        request: &T,
        settings: Option<toml::Value>,
    ) -> NativeServiceContext {
        let payload = bmux_plugin_sdk::encode_service_message(request)
            .expect("service request should encode");
        service_test_context_from_payload(config_dir, interface_id, operation, payload, settings)
    }

    fn cluster_settings_value() -> toml::Value {
        toml::from_str("[clusters.prod]\ntargets=['db-a','db-b']\n")
            .expect("cluster settings should parse")
    }

    struct ServiceTestHarness {
        fixture: ServiceTestConfigDir,
        plugin: ClusterPlugin,
    }

    impl ServiceTestHarness {
        fn new() -> Self {
            Self {
                fixture: ServiceTestConfigDir::create(),
                plugin: ClusterPlugin::default(),
            }
        }

        fn invoke<T: Serialize>(
            &self,
            interface_id: &str,
            operation: &str,
            request: &T,
        ) -> ServiceResponse {
            let context = service_test_context(
                self.fixture
                    .dir
                    .to_str()
                    .expect("config path should be utf-8"),
                interface_id,
                operation,
                request,
                Some(cluster_settings_value()),
            );
            self.plugin.invoke_service(context)
        }

        fn expect_error_code<T: Serialize>(
            &self,
            interface_id: &str,
            operation: &str,
            request: &T,
            expected_code: &str,
        ) {
            let response = self.invoke(interface_id, operation, request);
            let error = response.error.expect("service call should fail");
            assert_eq!(error.code, expected_code);
        }
    }

    #[test]
    fn durable_identity_ids_are_canonical_and_cryptographically_bound() {
        let runtime = FakeRuntime::default();
        let first_node = load_or_create_node_identity(&runtime).expect("create node identity");
        let second_node = load_or_create_node_identity(&runtime).expect("reload node identity");
        assert_eq!(first_node.node_id(), second_node.node_id());
        assert_eq!(first_node.public_key(), second_node.public_key());
        assert_eq!(
            first_node.node_id().to_string(),
            format!("node:{}", first_node.public_key())
        );
        assert_eq!(
            first_node
                .node_id()
                .to_string()
                .parse::<NodeId>()
                .expect("parse node id"),
            *first_node.node_id()
        );

        assert_eq!(load_cluster_id(&runtime).unwrap(), None);
        let initialized = initialize_cluster(&runtime).expect("initialize cluster");
        let cluster_text = initialized.cluster_id.clone().expect("cluster id");
        let first_cluster = cluster_text.parse::<ClusterId>().expect("parse cluster id");
        let second = initialize_cluster(&runtime).expect("reload initialized cluster");
        assert_eq!(second.cluster_id.as_deref(), Some(cluster_text.as_str()));
        assert_eq!(load_cluster_id(&runtime).unwrap(), Some(first_cluster));
        assert_eq!(initialized.node_id, first_node.node_id().to_string());
        assert_eq!(initialized.public_key, first_node.public_key().to_string());
        assert_eq!(initialized.capabilities, Some(initializer_capabilities()));
        let members = list_members(&runtime).unwrap();
        assert_eq!(members.members.len(), 1);
        assert_eq!(members.members[0].capabilities, initializer_capabilities());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn enrollment_rotation_revocation_and_member_revocation_fail_closed() {
        let issuer = FakeRuntime::default();
        let issuer_identity = load_or_create_node_identity(&issuer).unwrap();
        initialize_cluster(&issuer).unwrap();
        let joiner = FakeRuntime::default();
        let joiner_identity = load_or_create_node_identity(&joiner).unwrap();

        let revoked_token =
            create_enrollment_token(&issuer, "request-revoke", "issuer", Some(60_000), None)
                .unwrap();
        let revoked_signed =
            decode_and_verify_enrollment_token(&revoked_token.token, now_unix_ms()).unwrap();
        let enrollment_id = revoked_signed.claims.nonce.clone();
        assert_eq!(
            list_enrollments(&issuer).unwrap().enrollments[0].state,
            EnrollmentState::Active
        );
        assert!(revoke_enrollment(&issuer, &enrollment_id).unwrap().revoked);
        assert!(!revoke_enrollment(&issuer, &enrollment_id).unwrap().revoked);
        assert_eq!(
            list_enrollments(&issuer).unwrap().enrollments[0].state,
            EnrollmentState::Revoked
        );
        let revoked_request =
            enrollment_request(&joiner, &revoked_signed, revoked_token.token, None);
        assert!(
            redeem_enrollment(&issuer, &revoked_request)
                .unwrap_err()
                .contains("revoked")
        );
        assert!(
            create_enrollment_token(&issuer, "request-revoke", "issuer", Some(60_000), None)
                .unwrap_err()
                .contains("new request_id")
        );

        let token =
            create_enrollment_token(&issuer, "request-active", "issuer", Some(60_000), None)
                .unwrap();
        let signed = decode_and_verify_enrollment_token(&token.token, now_unix_ms()).unwrap();
        let request = enrollment_request(&joiner, &signed, token.token.clone(), None);
        let enrollment_result = redeem_enrollment(&issuer, &request).unwrap();
        adopt_join_result(
            &joiner,
            &ClusterCommandJoinRequest {
                token: token.token,
                issuer: "issuer".to_string(),
                enrollment_result,
            },
        )
        .unwrap();

        let original_member = list_members(&joiner)
            .unwrap()
            .members
            .into_iter()
            .find(|member| member.node_id == joiner_identity.node_id().to_string())
            .unwrap();
        assert!(
            create_enrollment_token(
                &joiner,
                "observer-cannot-issue",
                "joiner",
                Some(60_000),
                None,
            )
            .unwrap_err()
            .contains("active voter")
        );
        let stale_challenge = create_peer_auth_challenge(
            &issuer,
            &ClusterPeerChallengeRequest {
                claimant_node_id: joiner_identity.node_id().to_string(),
            },
        )
        .unwrap();
        let rotation = prepare_credential_rotation(&joiner).unwrap();
        let mut forged_rotation = rotation.clone();
        forged_rotation.signature = "00".repeat(64);
        assert!(
            accept_credential_rotation(&issuer, &forged_rotation)
                .unwrap_err()
                .contains("signature verification failed")
        );
        let rotated = accept_credential_rotation(&issuer, &rotation)
            .unwrap()
            .member;
        assert!(
            accept_credential_rotation(&issuer, &rotation)
                .unwrap_err()
                .contains("stale credential")
        );
        let stale_proof = create_peer_auth_proof(&joiner, stale_challenge).unwrap();
        assert!(
            authenticate_peer(
                &issuer,
                &ClusterPeerAuthenticateRequest { proof: stale_proof }
            )
            .unwrap_err()
            .contains("stale claimant credential")
        );
        commit_credential_rotation(&joiner, &rotated).unwrap();
        assert_ne!(rotated.credential_serial, original_member.credential_serial);
        assert_eq!(rotated.public_key, original_member.public_key);
        verify_membership_credential(&rotated, now_unix_ms()).unwrap();

        let revoked = revoke_member(&issuer, &joiner_identity.node_id().to_string())
            .unwrap()
            .member;
        assert_eq!(revoked.state, ClusterMemberState::Revoked);
        let retried = revoke_member(&issuer, &joiner_identity.node_id().to_string())
            .unwrap()
            .member;
        assert_eq!(retried, revoked);
        assert!(
            create_peer_auth_challenge(
                &issuer,
                &ClusterPeerChallengeRequest {
                    claimant_node_id: joiner_identity.node_id().to_string()
                }
            )
            .unwrap_err()
            .contains("not active")
        );
        assert!(
            revoke_member(&issuer, &issuer_identity.node_id().to_string())
                .unwrap_err()
                .contains("self-revocation")
        );
    }

    #[test]
    fn membership_status_reports_trust_compatibility_and_non_authoritative_liveness() {
        let issuer = FakeRuntime::default();
        let issuer_identity = load_or_create_node_identity(&issuer).unwrap();
        initialize_cluster(&issuer).unwrap();
        let joiner = FakeRuntime::default();
        let joiner_identity = load_or_create_node_identity(&joiner).unwrap();
        complete_test_join(&issuer, &joiner, "membership-status-join");

        let status = membership_status(&issuer).unwrap();
        let local = status
            .members
            .iter()
            .find(|member| member.member.node_id == issuer_identity.node_id().to_string())
            .unwrap();
        assert_eq!(local.liveness, MemberLivenessState::Local);
        assert_eq!(local.reachable, Some(true));
        assert!(local.compatible);
        assert!(local.trusted);
        let remote = status
            .members
            .iter()
            .find(|member| member.member.node_id == joiner_identity.node_id().to_string())
            .unwrap();
        assert_eq!(remote.liveness, MemberLivenessState::Unchecked);
        assert_eq!(remote.reachable, None);
        assert_eq!(remote.member.state, ClusterMemberState::Active);

        let mut state = load_membership_state(&issuer).unwrap().unwrap();
        let remote = state
            .members
            .get_mut(&joiner_identity.node_id().to_string())
            .unwrap();
        remote.credential_signature = "00".repeat(64);
        store_membership_state(&issuer, &state).unwrap();
        let status = membership_status(&issuer).unwrap();
        let remote = status
            .members
            .iter()
            .find(|member| member.member.node_id == joiner_identity.node_id().to_string())
            .unwrap();
        assert_eq!(remote.liveness, MemberLivenessState::Untrusted);
        assert!(!remote.trusted);
        assert_eq!(remote.member.state, ClusterMemberState::Active);
    }

    #[test]
    fn expired_credential_rotation_is_rejected_without_state_change() {
        let issuer = FakeRuntime::default();
        load_or_create_node_identity(&issuer).unwrap();
        initialize_cluster(&issuer).unwrap();
        let joiner = FakeRuntime::default();
        let joiner_identity = load_or_create_node_identity(&joiner).unwrap();
        complete_test_join(&issuer, &joiner, "rotation-expiry-join");
        let original = list_members(&issuer)
            .unwrap()
            .members
            .into_iter()
            .find(|member| member.node_id == joiner_identity.node_id().to_string())
            .unwrap();
        let mut rotation = prepare_credential_rotation(&joiner).unwrap();
        rotation.expires_at_unix_ms = now_unix_ms().saturating_sub(1);
        let claims = CredentialRotationClaims {
            version: CLUSTER_IDENTITY_VERSION,
            cluster_id: original.cluster_id.clone(),
            node_id: rotation.node_id.clone(),
            current_serial: rotation.current_serial.clone(),
            nonce: rotation.nonce.clone(),
            issued_at_unix_ms: rotation.issued_at_unix_ms,
            expires_at_unix_ms: rotation.expires_at_unix_ms,
        };
        rotation.signature = encode_hex(
            &joiner_identity.sign(&canonical_credential_rotation_claims(&claims).unwrap()),
        );
        assert!(
            accept_credential_rotation(&issuer, &rotation)
                .unwrap_err()
                .contains("expired")
        );
        let unchanged = list_members(&issuer)
            .unwrap()
            .members
            .into_iter()
            .find(|member| member.node_id == joiner_identity.node_id().to_string())
            .unwrap();
        assert_eq!(unchanged.credential_serial, original.credential_serial);
    }

    #[test]
    fn enrollment_token_is_signed_single_use_and_idempotent_for_same_node() {
        let issuer = FakeRuntime::default();
        load_or_create_node_identity(&issuer).unwrap();
        let identity = initialize_cluster(&issuer).unwrap();
        let token =
            create_enrollment_token(&issuer, "request-single-use", "issuer", Some(60_000), None)
                .unwrap();
        let retried =
            create_enrollment_token(&issuer, "request-single-use", "issuer", Some(60_000), None)
                .unwrap();
        assert_eq!(retried, token);
        let signed = decode_and_verify_enrollment_token(&token.token, now_unix_ms()).unwrap();
        assert_eq!(signed.claims.cluster_id, identity.cluster_id.unwrap());
        assert_eq!(signed.claims.capabilities, default_join_capabilities());

        let joiner = FakeRuntime::default();
        let request = enrollment_request(
            &joiner,
            &signed,
            token.token.clone(),
            Some("joiner".to_string()),
        );
        let first = redeem_enrollment(&issuer, &request).unwrap();
        let second = redeem_enrollment(&issuer, &request).unwrap();
        assert_eq!(first.member, second.member);
        assert_eq!(first.member.capabilities, default_join_capabilities());
        verify_membership_credential(&first.member, now_unix_ms()).unwrap();
        assert_eq!(first.member.credential_issuer_node_id, identity.node_id);
        assert_eq!(first.members, second.members);
        assert_eq!(list_members(&issuer).unwrap().members.len(), 2);

        let attacker_runtime = FakeRuntime::default();
        let replay = enrollment_request(&attacker_runtime, &signed, token.token, None);
        let error = redeem_enrollment(&issuer, &replay).expect_err("cross-node replay must fail");
        assert!(error.contains("already consumed by another node"));
    }

    #[test]
    fn enrollment_requires_node_possession_and_compatible_protocol_before_membership_mutation() {
        let issuer = FakeRuntime::default();
        load_or_create_node_identity(&issuer).unwrap();
        initialize_cluster(&issuer).unwrap();
        let token =
            create_enrollment_token(&issuer, "request-auth", "issuer", Some(60_000), None).unwrap();
        let signed = decode_and_verify_enrollment_token(&token.token, now_unix_ms()).unwrap();
        let joiner = FakeRuntime::default();
        let valid = enrollment_request(&joiner, &signed, token.token, None);

        let mut forged = valid.clone();
        forged.possession_signature = vec![0; forged.possession_signature.len()];
        assert!(
            redeem_enrollment(&issuer, &forged)
                .unwrap_err()
                .contains("possession proof verification failed")
        );
        assert_eq!(list_members(&issuer).unwrap().members.len(), 1);

        let mut incompatible_epoch = valid.clone();
        incompatible_epoch.protocol.wire_epoch += 1;
        incompatible_epoch.possession_signature = create_enrollment_possession_proof(
            &joiner,
            &signed,
            incompatible_epoch.endpoint.clone(),
            &incompatible_epoch.protocol,
        )
        .unwrap();
        assert!(
            redeem_enrollment(&issuer, &incompatible_epoch)
                .unwrap_err()
                .contains("incompatible cluster wire epoch")
        );
        assert_eq!(list_members(&issuer).unwrap().members.len(), 1);

        let mut incompatible_revision = valid.clone();
        incompatible_revision.protocol.peer_revision_min = 2;
        incompatible_revision.protocol.peer_revision_max = 2;
        incompatible_revision.possession_signature = create_enrollment_possession_proof(
            &joiner,
            &signed,
            incompatible_revision.endpoint.clone(),
            &incompatible_revision.protocol,
        )
        .unwrap();
        assert!(
            redeem_enrollment(&issuer, &incompatible_revision)
                .unwrap_err()
                .contains("no compatible cluster peer revision")
        );
        assert_eq!(list_members(&issuer).unwrap().members.len(), 1);

        let mut incompatible_schema = valid.clone();
        incompatible_schema.protocol.schema_version_min = 2;
        incompatible_schema.protocol.schema_version_max = 2;
        incompatible_schema.possession_signature = create_enrollment_possession_proof(
            &joiner,
            &signed,
            incompatible_schema.endpoint.clone(),
            &incompatible_schema.protocol,
        )
        .unwrap();
        assert!(
            redeem_enrollment(&issuer, &incompatible_schema)
                .unwrap_err()
                .contains("no compatible cluster schema version")
        );
        assert_eq!(list_members(&issuer).unwrap().members.len(), 1);

        let mut missing_feature = valid.clone();
        missing_feature.protocol.features.clear();
        missing_feature.possession_signature = create_enrollment_possession_proof(
            &joiner,
            &signed,
            missing_feature.endpoint.clone(),
            &missing_feature.protocol,
        )
        .unwrap();
        assert!(
            redeem_enrollment(&issuer, &missing_feature)
                .unwrap_err()
                .contains("missing mandatory cluster feature")
        );
        assert_eq!(list_members(&issuer).unwrap().members.len(), 1);

        let accepted = redeem_enrollment(&issuer, &valid).unwrap();
        assert_eq!(accepted.member.negotiated_protocol.peer_revision, 1);
        assert_eq!(accepted.member.negotiated_protocol.schema_version, 1);
        assert_eq!(list_members(&issuer).unwrap().members.len(), 2);
    }

    #[test]
    fn membership_credential_rejects_tampering_and_expiry() {
        let issuer = FakeRuntime::default();
        load_or_create_node_identity(&issuer).unwrap();
        initialize_cluster(&issuer).unwrap();
        let token =
            create_enrollment_token(&issuer, "request-credential", "issuer", Some(60_000), None)
                .unwrap();
        let signed = decode_and_verify_enrollment_token(&token.token, now_unix_ms()).unwrap();
        let joiner = FakeRuntime::default();
        let request = enrollment_request(&joiner, &signed, token.token, None);
        let credential = redeem_enrollment(&issuer, &request).unwrap().member;
        verify_membership_credential(&credential, credential.credential_issued_at_unix_ms).unwrap();

        let mut tampered = credential.clone();
        tampered.capabilities.ingress = !tampered.capabilities.ingress;
        assert!(
            verify_membership_credential(&tampered, now_unix_ms())
                .unwrap_err()
                .contains("signature verification failed")
        );
        assert!(
            verify_membership_credential(
                &credential,
                credential.credential_expires_at_unix_ms.saturating_add(1)
            )
            .unwrap_err()
            .contains("expired")
        );
    }

    #[test]
    fn peer_authentication_is_mutual_scoped_single_use_and_credential_bound() {
        let verifier = FakeRuntime::default();
        load_or_create_node_identity(&verifier).unwrap();
        initialize_cluster(&verifier).unwrap();
        let claimant = FakeRuntime::default();
        let claimant_identity = load_or_create_node_identity(&claimant).unwrap();
        complete_test_join(&verifier, &claimant, "peer-auth-join");
        let verifier_identity = load_or_create_node_identity(&verifier).unwrap();

        let challenge = create_peer_auth_challenge(
            &verifier,
            &ClusterPeerChallengeRequest {
                claimant_node_id: claimant_identity.node_id().to_string(),
            },
        )
        .unwrap();
        assert_eq!(
            challenge.audience_node_id,
            claimant_identity.node_id().to_string()
        );
        assert_eq!(
            challenge.verifier_node_id,
            verifier_identity.node_id().to_string()
        );
        let proof = create_peer_auth_proof(&claimant, challenge.clone()).unwrap();
        let authenticated = authenticate_peer(
            &verifier,
            &ClusterPeerAuthenticateRequest {
                proof: proof.clone(),
            },
        )
        .unwrap();
        assert_eq!(
            authenticated.node_id,
            claimant_identity.node_id().to_string()
        );
        assert_eq!(authenticated.cluster_id, challenge.cluster_id);
        assert!(
            authenticate_peer(&verifier, &ClusterPeerAuthenticateRequest { proof })
                .unwrap_err()
                .contains("already consumed")
        );

        let wrong_audience = create_peer_auth_challenge(
            &verifier,
            &ClusterPeerChallengeRequest {
                claimant_node_id: claimant_identity.node_id().to_string(),
            },
        )
        .unwrap();
        assert!(
            create_peer_auth_proof(&verifier, wrong_audience)
                .unwrap_err()
                .contains("wrong audience")
        );

        let mut tampered = create_peer_auth_challenge(
            &verifier,
            &ClusterPeerChallengeRequest {
                claimant_node_id: claimant_identity.node_id().to_string(),
            },
        )
        .unwrap();
        tampered.nonce.push('x');
        assert!(
            create_peer_auth_proof(&claimant, tampered)
                .unwrap_err()
                .contains("signature verification failed")
        );
    }

    #[test]
    fn peer_authentication_rejects_forged_expired_stale_and_inactive_credentials() {
        let verifier = FakeRuntime::default();
        load_or_create_node_identity(&verifier).unwrap();
        initialize_cluster(&verifier).unwrap();
        let claimant = FakeRuntime::default();
        let claimant_identity = load_or_create_node_identity(&claimant).unwrap();
        complete_test_join(&verifier, &claimant, "peer-auth-negative-join");

        let challenge = create_peer_auth_challenge(
            &verifier,
            &ClusterPeerChallengeRequest {
                claimant_node_id: claimant_identity.node_id().to_string(),
            },
        )
        .unwrap();
        let mut forged = create_peer_auth_proof(&claimant, challenge).unwrap();
        forged.claimant_signature = "00".repeat(forged.claimant_signature.len() / 2);
        assert!(
            authenticate_peer(&verifier, &ClusterPeerAuthenticateRequest { proof: forged })
                .unwrap_err()
                .contains("proof signature verification failed")
        );

        let mut expired = create_peer_auth_challenge(
            &verifier,
            &ClusterPeerChallengeRequest {
                claimant_node_id: claimant_identity.node_id().to_string(),
            },
        )
        .unwrap();
        expired.expires_at_unix_ms = now_unix_ms().saturating_sub(1);
        expired.signature = encode_hex(
            &load_or_create_node_identity(&verifier)
                .unwrap()
                .sign(&canonical_peer_challenge(&expired).unwrap()),
        );
        assert!(
            create_peer_auth_proof(&claimant, expired)
                .unwrap_err()
                .contains("expired")
        );

        let challenge = create_peer_auth_challenge(
            &verifier,
            &ClusterPeerChallengeRequest {
                claimant_node_id: claimant_identity.node_id().to_string(),
            },
        )
        .unwrap();
        let mut stale = create_peer_auth_proof(&claimant, challenge).unwrap();
        stale.claimant_credential_serial = "stale".to_string();
        stale.claimant_signature = encode_hex(
            &load_or_create_node_identity(&claimant)
                .unwrap()
                .sign(&canonical_peer_proof(&stale).unwrap()),
        );
        assert!(
            authenticate_peer(&verifier, &ClusterPeerAuthenticateRequest { proof: stale })
                .unwrap_err()
                .contains("stale claimant credential")
        );

        let leave_id = Uuid::new_v4().to_string();
        let cluster_id = list_members(&verifier).unwrap().cluster_id.unwrap();
        let claims = LeaveClaims {
            version: CLUSTER_IDENTITY_VERSION,
            leave_id: leave_id.clone(),
            cluster_id: cluster_id.clone(),
            node_id: claimant_identity.node_id().to_string(),
        };
        accept_leave(
            &verifier,
            &ClusterCommandAcceptLeaveRequest {
                leave_id,
                cluster_id,
                node_id: claimant_identity.node_id().to_string(),
                signature: claimant_identity.sign(&canonical_leave_claims(&claims).unwrap()),
            },
        )
        .unwrap();
        assert!(
            create_peer_auth_challenge(
                &verifier,
                &ClusterPeerChallengeRequest {
                    claimant_node_id: claimant_identity.node_id().to_string()
                }
            )
            .unwrap_err()
            .contains("not active")
        );
    }

    #[test]
    fn explicit_voter_worker_ingress_grant_is_signed_and_persisted() {
        let issuer = FakeRuntime::default();
        load_or_create_node_identity(&issuer).unwrap();
        initialize_cluster(&issuer).unwrap();
        let grant = ClusterNodeCapabilities {
            consensus_role: ClusterConsensusRole::Voter,
            worker: false,
            ingress: true,
        };
        let token = create_enrollment_token(
            &issuer,
            "request-voter",
            "issuer",
            Some(60_000),
            Some(grant.clone()),
        )
        .unwrap();
        let signed = decode_and_verify_enrollment_token(&token.token, now_unix_ms()).unwrap();
        assert_eq!(signed.claims.capabilities, grant);

        let joiner_runtime = FakeRuntime::default();
        let joiner = load_or_create_node_identity(&joiner_runtime).unwrap();
        let request = enrollment_request(
            &joiner_runtime,
            &signed,
            token.token,
            Some("voter-b".to_string()),
        );
        let result = redeem_enrollment(&issuer, &request).unwrap();
        assert_eq!(result.member.capabilities, grant);
        assert_eq!(
            list_members(&issuer)
                .unwrap()
                .members
                .into_iter()
                .find(|member| member.node_id == joiner.node_id().to_string())
                .unwrap()
                .capabilities,
            grant
        );
    }

    #[test]
    fn legacy_membership_records_gain_deterministic_capability_metadata() {
        let runtime = FakeRuntime::default();
        let local = load_or_create_node_identity(&runtime).unwrap();
        let identity = initialize_cluster(&runtime).unwrap();
        let bytes = runtime.storage_value(MEMBERSHIP_STATE_STORAGE_KEY).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let members = value["members"].as_object_mut().unwrap();
        members
            .get_mut(&local.node_id().to_string())
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("capabilities");
        let remote_key = iroh::SecretKey::generate();
        let remote_id = NodeId::from_secret_key(&remote_key).to_string();
        members.insert(
            remote_id.clone(),
            serde_json::json!({
                "cluster_id": identity.cluster_id.unwrap(),
                "node_id": remote_id,
                "public_key": remote_key.public().to_string(),
                "endpoint": "legacy-remote",
                "joined_at_unix_ms": 1,
                "updated_at_unix_ms": 1,
                "state": "active"
            }),
        );
        runtime.set_storage_value(
            MEMBERSHIP_STATE_STORAGE_KEY,
            serde_json::to_vec(&value).unwrap(),
        );

        let members = list_members(&runtime).unwrap().members;
        assert_eq!(
            members
                .iter()
                .find(|member| member.node_id == local.node_id().to_string())
                .unwrap()
                .capabilities,
            initializer_capabilities()
        );
        assert_eq!(
            members
                .iter()
                .find(|member| member.node_id == remote_id)
                .unwrap()
                .capabilities,
            default_join_capabilities()
        );
        let persisted: serde_json::Value =
            serde_json::from_slice(&runtime.storage_value(MEMBERSHIP_STATE_STORAGE_KEY).unwrap())
                .unwrap();
        assert!(
            persisted["members"]
                .as_object()
                .unwrap()
                .values()
                .all(|member| member.get("capabilities").is_some())
        );
    }

    #[test]
    fn enrollment_capability_flags_are_validated() {
        assert_eq!(
            parse_enrollment_capabilities(&[]).unwrap(),
            default_join_capabilities()
        );
        assert_eq!(
            parse_enrollment_capabilities(&[
                "--role".to_string(),
                "voter".to_string(),
                "--no-worker".to_string(),
                "--ingress".to_string(),
            ])
            .unwrap(),
            ClusterNodeCapabilities {
                consensus_role: ClusterConsensusRole::Voter,
                worker: false,
                ingress: true,
            }
        );
        assert!(
            parse_enrollment_capabilities(&["--worker".to_string(), "--no-worker".to_string(),])
                .unwrap_err()
                .contains("conflicts")
        );
        assert!(
            parse_enrollment_capabilities(&["--role".to_string(), "leader".to_string(),])
                .unwrap_err()
                .contains("invalid --role")
        );
    }

    #[test]
    fn enrollment_token_rejects_tampering_expiry_and_identity_mismatch() {
        let issuer = FakeRuntime::default();
        load_or_create_node_identity(&issuer).unwrap();
        initialize_cluster(&issuer).unwrap();
        let token =
            create_enrollment_token(&issuer, "request-expiry", "issuer", Some(1), None).unwrap();
        let signed = decode_and_verify_enrollment_token(&token.token, token.expires_at_unix_ms)
            .expect("token valid at expiry");
        assert!(
            decode_and_verify_enrollment_token(
                &token.token,
                token.expires_at_unix_ms.saturating_add(1)
            )
            .unwrap_err()
            .contains("expired")
        );

        let mut tampered = signed;
        tampered.claims.issuer_endpoint = "attacker".to_string();
        let tampered = encode_enrollment_token(&tampered).unwrap();
        assert!(
            decode_and_verify_enrollment_token(&tampered, token.expires_at_unix_ms)
                .unwrap_err()
                .contains("signature verification failed")
        );

        let valid_token =
            create_enrollment_token(&issuer, "request-mismatch", "issuer", Some(60_000), None)
                .unwrap();
        let valid_signed =
            decode_and_verify_enrollment_token(&valid_token.token, now_unix_ms()).unwrap();
        let joiner_runtime = FakeRuntime::default();
        let joiner = load_or_create_node_identity(&joiner_runtime).unwrap();
        let other = load_or_create_node_identity(&FakeRuntime::default()).unwrap();
        let protocol = current_protocol_offer();
        let possession_signature =
            create_enrollment_possession_proof(&joiner_runtime, &valid_signed, None, &protocol)
                .unwrap();
        let request = ClusterCommandRedeemEnrollmentRequest {
            token: valid_token.token.clone(),
            node_id: joiner.node_id().to_string(),
            public_key: other.public_key().to_string(),
            endpoint: None,
            protocol,
            possession_signature,
        };
        assert!(
            redeem_enrollment(&issuer, &request)
                .unwrap_err()
                .contains("does not match joining public key")
        );

        let mut overlong_offer = current_protocol_offer();
        overlong_offer.plugin_version = "x".repeat(129);
        let overlong_signature = create_enrollment_possession_proof(
            &joiner_runtime,
            &valid_signed,
            None,
            &overlong_offer,
        )
        .unwrap();
        let overlong_request = ClusterCommandRedeemEnrollmentRequest {
            token: valid_token.token,
            node_id: joiner.node_id().to_string(),
            public_key: joiner.public_key().to_string(),
            endpoint: None,
            protocol: overlong_offer,
            possession_signature: overlong_signature,
        };
        assert!(
            redeem_enrollment(&issuer, &overlong_request)
                .unwrap_err()
                .contains("plugin version is invalid")
        );
        assert_eq!(list_members(&issuer).unwrap().members.len(), 1);
    }

    #[test]
    fn signed_leave_is_idempotent_and_rejects_forgery() {
        let issuer = FakeRuntime::default();
        let issuer_identity = load_or_create_node_identity(&issuer).unwrap();
        let identity = initialize_cluster(&issuer).unwrap();
        let cluster_id = identity.cluster_id.unwrap();
        let leave_id = Uuid::new_v4().to_string();
        let claims = LeaveClaims {
            version: CLUSTER_IDENTITY_VERSION,
            leave_id: leave_id.clone(),
            cluster_id: cluster_id.clone(),
            node_id: issuer_identity.node_id().to_string(),
        };
        let forged = ClusterCommandAcceptLeaveRequest {
            leave_id: leave_id.clone(),
            cluster_id: cluster_id.clone(),
            node_id: issuer_identity.node_id().to_string(),
            signature: iroh::SecretKey::generate()
                .sign(&canonical_leave_claims(&claims).unwrap())
                .to_bytes()
                .to_vec(),
        };
        assert!(
            accept_leave(&issuer, &forged)
                .unwrap_err()
                .contains("signature verification failed")
        );

        let valid = ClusterCommandAcceptLeaveRequest {
            leave_id,
            cluster_id,
            node_id: issuer_identity.node_id().to_string(),
            signature: issuer_identity.sign(&canonical_leave_claims(&claims).unwrap()),
        };
        assert!(accept_leave(&issuer, &valid).unwrap().left);
        assert!(accept_leave(&issuer, &valid).unwrap().left);
        assert_eq!(
            list_members(&issuer).unwrap().members[0].state,
            ClusterMemberState::Left
        );
    }

    #[test]
    fn leave_commit_requires_matching_prepared_transaction_and_preserves_node_key() {
        let runtime = FakeRuntime::default();
        let node = load_or_create_node_identity(&runtime).unwrap();
        initialize_cluster(&runtime).unwrap();
        let prepared = prepare_leave(&runtime).unwrap();
        assert!(
            commit_leave(&runtime, "wrong-leave-id")
                .unwrap_err()
                .contains("does not match")
        );
        let result = commit_leave(&runtime, &prepared.leave_id).unwrap();
        assert_eq!(result.node_id, node.node_id().to_string());
        assert!(result.left);
        assert_eq!(load_cluster_id(&runtime).unwrap(), None);
        assert!(
            load_identity_record(&runtime, MEMBERSHIP_STATE_STORAGE_KEY)
                .unwrap()
                .is_none()
        );
        assert!(
            load_identity_record(&runtime, PENDING_LEAVE_STORAGE_KEY)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            load_node_identity(&runtime).unwrap().unwrap().node_id(),
            node.node_id()
        );
    }

    #[test]
    fn independently_created_node_identities_are_distinct() {
        let first = load_or_create_node_identity(&FakeRuntime::default()).unwrap();
        let second = load_or_create_node_identity(&FakeRuntime::default()).unwrap();
        assert_ne!(first.node_id(), second.node_id());
        assert_ne!(first.public_key(), second.public_key());
    }

    #[test]
    fn corrupt_identity_records_fail_closed_without_rotation() {
        let runtime = FakeRuntime::default();
        runtime.set_storage_value(NODE_IDENTITY_STORAGE_KEY, b"not-json".to_vec());
        let before = runtime.storage_value(NODE_IDENTITY_STORAGE_KEY).unwrap();
        let error = load_or_create_node_identity(&runtime).expect_err("corruption must fail");
        assert!(error.contains("corrupt"));
        assert_eq!(
            runtime.storage_value(NODE_IDENTITY_STORAGE_KEY).unwrap(),
            before
        );

        runtime.set_storage_value(
            CLUSTER_ID_STORAGE_KEY,
            serde_json::to_vec(&StoredClusterIdentity {
                version: CLUSTER_IDENTITY_VERSION + 1,
                cluster_id: ClusterId::generate().to_string(),
            })
            .unwrap(),
        );
        let error = load_cluster_id(&runtime).expect_err("future version must fail");
        assert!(error.contains("unsupported cluster identity record version"));
    }

    #[test]
    fn tampered_node_identity_components_are_rejected() {
        let runtime = FakeRuntime::default();
        let identity = load_or_create_node_identity(&runtime).unwrap();
        let bytes = runtime.storage_value(NODE_IDENTITY_STORAGE_KEY).unwrap();
        let mut record: StoredNodeIdentity = serde_json::from_slice(&bytes).unwrap();
        record.public_key = iroh::SecretKey::generate().public().to_string();
        runtime.set_storage_value(
            NODE_IDENTITY_STORAGE_KEY,
            serde_json::to_vec(&record).unwrap(),
        );
        let error = load_node_identity(&runtime).expect_err("public-key mismatch must fail");
        assert!(error.contains("public key does not match private key"));

        record.public_key = identity.public_key().to_string();
        record.node_id = NodeId::from_secret_key(&iroh::SecretKey::generate()).to_string();
        runtime.set_storage_value(
            NODE_IDENTITY_STORAGE_KEY,
            serde_json::to_vec(&record).unwrap(),
        );
        let error = load_node_identity(&runtime).expect_err("node-id mismatch must fail");
        assert!(error.contains("node ID does not match"));
    }

    #[test]
    fn invalid_identity_text_is_rejected() {
        assert!(
            "cluster:00000000-0000-0000-0000-000000000000"
                .parse::<ClusterId>()
                .is_err()
        );
        assert!("node:not-a-public-key".parse::<NodeId>().is_err());
        assert!(
            "wrong:0194f776-7c0d-7000-8000-000000000000"
                .parse::<ClusterId>()
                .is_err()
        );
    }

    #[test]
    fn target_from_host_ref_accepts_string_variant() {
        let host = ClusterHostRef::Target("prod-a".to_string());
        assert_eq!(target_from_host_ref(&host).as_deref(), Some("prod-a"));
    }

    #[test]
    fn invoke_service_list_clusters_returns_inventory_from_settings() {
        let harness = ServiceTestHarness::new();
        let response = harness.invoke("cluster-query/v1", "list_clusters", &());
        assert!(
            response.error.is_none(),
            "list_clusters should succeed: {:?}",
            response.error
        );
        let decoded: ClusterQueryListClustersResponse =
            bmux_plugin_sdk::decode_service_message(&response.payload)
                .expect("list_clusters response should decode");
        assert_eq!(
            decoded.clusters.get("prod").cloned(),
            Some(vec!["db-a".to_string(), "db-b".to_string()])
        );
    }

    #[test]
    fn invoke_service_status_returns_degraded_when_probe_runtime_is_unavailable() {
        let harness = ServiceTestHarness::new();
        let response = harness.invoke(
            "cluster-query/v1",
            "status",
            &ClusterQueryStatusRequest {
                selector: Some("prod".to_string()),
                doctor: Some(false),
            },
        );
        assert!(response.error.is_none(), "status should succeed");
        let decoded: ClusterQueryStatusResponse =
            bmux_plugin_sdk::decode_service_message(&response.payload)
                .expect("status response should decode");
        assert_eq!(decoded.statuses.len(), 2);
        assert!(
            decoded
                .statuses
                .iter()
                .all(|status| matches!(status.state, ClusterHostState::Degraded))
        );
    }

    #[test]
    fn invoke_service_up_maps_runtime_failures_to_up_failed() {
        let harness = ServiceTestHarness::new();
        harness.expect_error_code(
            "cluster-command/v1",
            "up",
            &ClusterCommandUpRequest {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
            },
            "up_failed",
        );
    }

    #[test]
    fn invoke_service_pane_new_maps_runtime_failures_to_pane_new_failed() {
        let harness = ServiceTestHarness::new();
        harness.expect_error_code(
            "cluster-command/v1",
            "pane_new",
            &ClusterCommandPaneNewRequest {
                host: "db-a".to_string(),
                name: None,
            },
            "pane_new_failed",
        );
    }

    #[test]
    fn invoke_service_pane_retry_maps_runtime_failures_to_pane_retry_failed() {
        let harness = ServiceTestHarness::new();
        harness.expect_error_code(
            "cluster-command/v1",
            "pane_retry",
            &ClusterCommandPaneRetryRequest { pane: None },
            "pane_retry_failed",
        );
    }

    #[test]
    fn invoke_service_pane_move_maps_runtime_failures_to_pane_move_failed() {
        let harness = ServiceTestHarness::new();
        harness.expect_error_code(
            "cluster-command/v1",
            "pane_move",
            &ClusterCommandPaneMoveRequest {
                pane: None,
                host: "db-b".to_string(),
            },
            "pane_move_failed",
        );
    }

    #[test]
    fn invoke_service_events_list_maps_runtime_failures_to_connection_events_list_failed() {
        let harness = ServiceTestHarness::new();
        harness.expect_error_code(
            "cluster-connection-events/v1",
            "list",
            &(),
            "connection_events_list_failed",
        );
    }

    #[test]
    fn target_from_host_ref_accepts_object_fields() {
        let host = ClusterHostRef::Object {
            target: None,
            host: Some("prod-b".to_string()),
            name: None,
        };
        assert_eq!(target_from_host_ref(&host).as_deref(), Some("prod-b"));
    }

    #[test]
    fn dedupe_preserve_order_keeps_first_position() {
        let deduped = dedupe_preserve_order(vec![
            "a".to_string(),
            "b".to_string(),
            "a".to_string(),
            "c".to_string(),
        ]);
        assert_eq!(deduped, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_cluster_up_args_extracts_cluster_and_hosts() {
        let parsed = parse_cluster_up_args(&[
            "prod".to_string(),
            "--host".to_string(),
            "db-a".to_string(),
            "--host=db-b".to_string(),
            "cache-a".to_string(),
        ])
        .expect("arguments should parse");

        assert_eq!(parsed.cluster, "prod");
        assert_eq!(parsed.hosts, vec!["db-a", "db-b", "cache-a"]);
        assert_eq!(parsed.on_failure, RetryFailurePolicy::Continue);
        assert_eq!(parsed.retries, 0);
    }

    #[test]
    fn parse_cluster_up_args_supports_failure_policy_and_retries() {
        let parsed = parse_cluster_up_args(&[
            "prod".to_string(),
            "--on-failure".to_string(),
            "prompt".to_string(),
            "--retries".to_string(),
            "2".to_string(),
        ])
        .expect("arguments should parse");

        assert_eq!(parsed.cluster, "prod");
        assert!(parsed.hosts.is_empty());
        assert_eq!(parsed.on_failure, RetryFailurePolicy::Prompt);
        assert_eq!(parsed.retries, 2);
    }

    #[test]
    fn parse_cluster_up_args_supports_abort_policy() {
        let parsed = parse_cluster_up_args(&["prod".to_string(), "--on-failure=abort".to_string()])
            .expect("arguments should parse");
        assert_eq!(parsed.on_failure, RetryFailurePolicy::Abort);
    }

    #[test]
    fn parse_cluster_up_args_requires_cluster() {
        let error = parse_cluster_up_args(&["--host".to_string(), "db-a".to_string()])
            .expect_err("cluster argument should be required");
        assert!(error.contains("requires CLUSTER"));
    }

    #[test]
    fn parse_cluster_pane_new_args_parses_flags_and_aliases() {
        let parsed = parse_cluster_pane_new_args(&[
            "--host".to_string(),
            "db-a".to_string(),
            "-n".to_string(),
            "primary-db".to_string(),
        ])
        .expect("arguments should parse");

        assert_eq!(parsed.host, "db-a");
        assert_eq!(parsed.name.as_deref(), Some("primary-db"));
    }

    #[test]
    fn parse_cluster_pane_new_args_accepts_positional_host() {
        let parsed = parse_cluster_pane_new_args(&["cache-a".to_string()])
            .expect("positional host should parse");
        assert_eq!(parsed.host, "cache-a");
        assert_eq!(parsed.name, None);
    }

    #[test]
    fn parse_cluster_pane_new_args_requires_host() {
        let error = parse_cluster_pane_new_args(&["--name".to_string(), "x".to_string()])
            .expect_err("host should be required");
        assert!(error.contains("requires --host"));
    }

    #[test]
    fn parse_cluster_pane_retry_args_defaults_to_active() {
        let parsed = parse_cluster_pane_retry_args(&[]).expect("retry args should parse");
        assert!(matches!(parsed.pane, PaneRetryRef::Active));
        assert_eq!(parsed.on_failure, RetryFailurePolicy::Abort);
        assert_eq!(parsed.retries, 0);
    }

    #[test]
    fn parse_cluster_pane_retry_args_supports_index() {
        let parsed = parse_cluster_pane_retry_args(&["--pane".to_string(), "3".to_string()])
            .expect("retry args should parse");
        assert!(matches!(parsed.pane, PaneRetryRef::Index(3)));
    }

    #[test]
    fn parse_cluster_pane_retry_args_supports_policy_and_retry_count() {
        let parsed = parse_cluster_pane_retry_args(&[
            "--pane".to_string(),
            "active".to_string(),
            "--on-failure".to_string(),
            "prompt".to_string(),
            "--retries".to_string(),
            "2".to_string(),
        ])
        .expect("retry args should parse");
        assert_eq!(parsed.on_failure, RetryFailurePolicy::Prompt);
        assert_eq!(parsed.retries, 2);
    }

    #[test]
    fn parse_cluster_pane_retry_args_supports_continue_policy() {
        let parsed = parse_cluster_pane_retry_args(&[
            "--on-failure=continue".to_string(),
            "--retries=1".to_string(),
        ])
        .expect("retry args should parse");
        assert_eq!(parsed.on_failure, RetryFailurePolicy::Continue);
        assert_eq!(parsed.retries, 1);
    }

    #[test]
    fn parse_cluster_events_args_defaults_to_text() {
        let parsed = parse_cluster_events_args(&[]).expect("events args should parse");
        assert_eq!(parsed.format, ClusterEventsFormat::Text);
        assert_eq!(parsed.cluster, None);
        assert_eq!(parsed.target, None);
        assert_eq!(parsed.state, None);
        assert_eq!(parsed.since_unix_ms, None);
        assert_eq!(parsed.limit, None);
    }

    #[test]
    fn parse_cluster_events_args_supports_filters() {
        let parsed = parse_cluster_events_args(&[
            "--format".to_string(),
            "json".to_string(),
            "--cluster".to_string(),
            "prod".to_string(),
            "--target".to_string(),
            "db-a".to_string(),
            "--state".to_string(),
            "retrying".to_string(),
            "--since".to_string(),
            "1712345678000".to_string(),
            "--limit".to_string(),
            "25".to_string(),
        ])
        .expect("events args should parse");
        assert_eq!(parsed.format, ClusterEventsFormat::Json);
        assert_eq!(parsed.cluster.as_deref(), Some("prod"));
        assert_eq!(parsed.target.as_deref(), Some("db-a"));
        assert_eq!(parsed.state, Some(ClusterConnectionState::Retrying));
        assert_eq!(parsed.since_unix_ms, Some(1_712_345_678_000));
        assert_eq!(parsed.limit, Some(25));
    }

    #[test]
    fn parse_cluster_events_args_rejects_zero_limit() {
        let error = parse_cluster_events_args(&["--limit".to_string(), "0".to_string()])
            .expect_err("limit zero should be rejected");
        assert!(error.contains("greater than zero"));
    }

    #[test]
    fn parse_cluster_events_args_rejects_invalid_since() {
        let error = parse_cluster_events_args(&["--since".to_string(), "abc".to_string()])
            .expect_err("invalid since should be rejected");
        assert!(error.contains("relative duration"));
    }

    #[test]
    fn filter_cluster_events_applies_combined_filters_and_tail_limit() {
        let events = vec![
            ClusterConnectionEvent {
                ts_unix_ms: 10,
                pane_id: Some("p1".to_string()),
                cluster: Some("prod".to_string()),
                target: Some("db-a".to_string()),
                source: Some("up".to_string()),
                state: ClusterConnectionState::Connecting,
                message: "launching".to_string(),
            },
            ClusterConnectionEvent {
                ts_unix_ms: 20,
                pane_id: Some("p2".to_string()),
                cluster: Some("prod".to_string()),
                target: Some("db-a".to_string()),
                source: Some("retry".to_string()),
                state: ClusterConnectionState::Retrying,
                message: "retrying".to_string(),
            },
            ClusterConnectionEvent {
                ts_unix_ms: 30,
                pane_id: Some("p3".to_string()),
                cluster: Some("prod".to_string()),
                target: Some("db-a".to_string()),
                source: Some("retry".to_string()),
                state: ClusterConnectionState::Retrying,
                message: "retrying-again".to_string(),
            },
            ClusterConnectionEvent {
                ts_unix_ms: 40,
                pane_id: Some("p4".to_string()),
                cluster: Some("prod".to_string()),
                target: Some("db-b".to_string()),
                source: Some("up".to_string()),
                state: ClusterConnectionState::Failed,
                message: "failed".to_string(),
            },
        ];
        let args = ClusterEventsArgs {
            format: ClusterEventsFormat::Text,
            cluster: Some("prod".to_string()),
            target: Some("db-a".to_string()),
            state: Some(ClusterConnectionState::Retrying),
            since_unix_ms: Some(15),
            limit: Some(1),
        };

        let filtered = filter_cluster_events(events, &args);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].pane_id.as_deref(), Some("p3"));
        assert_eq!(filtered[0].message, "retrying-again");
    }

    #[test]
    fn filter_cluster_events_applies_since_cutoff() {
        let events = vec![
            ClusterConnectionEvent {
                ts_unix_ms: 100,
                pane_id: None,
                cluster: Some("prod".to_string()),
                target: Some("db-a".to_string()),
                source: Some("up".to_string()),
                state: ClusterConnectionState::Connecting,
                message: "old".to_string(),
            },
            ClusterConnectionEvent {
                ts_unix_ms: 200,
                pane_id: None,
                cluster: Some("prod".to_string()),
                target: Some("db-a".to_string()),
                source: Some("up".to_string()),
                state: ClusterConnectionState::Ready,
                message: "new".to_string(),
            },
        ];
        let args = ClusterEventsArgs {
            format: ClusterEventsFormat::Text,
            cluster: None,
            target: None,
            state: None,
            since_unix_ms: Some(150),
            limit: None,
        };

        let filtered = filter_cluster_events(events, &args);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].message, "new");
    }

    #[test]
    fn parse_cluster_events_since_accepts_relative_minutes() {
        let before = now_unix_ms();
        let parsed = parse_cluster_events_since("15m").expect("relative since should parse");
        let after = now_unix_ms();
        assert!(parsed <= after.saturating_sub(900_000));
        assert!(parsed >= before.saturating_sub(900_000));
    }

    #[test]
    fn parse_cluster_events_since_accepts_compound_duration() {
        let before = now_unix_ms();
        let parsed =
            parse_cluster_events_since("1h30m").expect("compound relative since should parse");
        let after = now_unix_ms();
        assert!(parsed <= after.saturating_sub(5_400_000));
        assert!(parsed >= before.saturating_sub(5_400_000));
    }

    #[test]
    fn parse_cluster_events_since_accepts_absolute_unix_ms() {
        let parsed =
            parse_cluster_events_since("1712345678000").expect("absolute unix ms should parse");
        assert_eq!(parsed, 1_712_345_678_000);
    }

    #[test]
    fn parse_cluster_events_since_accepts_now_aliases() {
        let before = now_unix_ms();
        let now_alias = parse_cluster_events_since("now").expect("now alias should parse");
        let zero_alias = parse_cluster_events_since("0").expect("zero alias should parse");
        let after = now_unix_ms();

        assert!(now_alias >= before && now_alias <= after);
        assert!(zero_alias >= before && zero_alias <= after);
    }

    #[test]
    fn parse_cluster_events_since_rejects_malformed_compound_duration() {
        let error = parse_cluster_events_since("1h30")
            .expect_err("malformed compound duration should be rejected");
        assert!(error.contains("missing a unit"));
    }

    #[test]
    fn execute_cluster_up_tracks_ready_and_degraded_hosts() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-a", true);
        runtime.set_health("db-b", true);
        runtime.fail_launch_for("db-b");

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([(
                "prod".to_string(),
                vec!["db-a".to_string(), "db-b".to_string()],
            )]),
            known_targets: BTreeSet::from(["db-a".to_string(), "db-b".to_string()]),
        };
        let result = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Continue,
                retries: 0,
            },
        )
        .expect("cluster up should complete with partial start");

        let ready = result
            .statuses
            .iter()
            .find(|status| status.target == "db-a")
            .expect("db-a status should exist");
        assert!(matches!(ready.state, ClusterHostState::Ready));
        assert!(ready.pane_id.is_some());

        let degraded = result
            .statuses
            .iter()
            .find(|status| status.target == "db-b")
            .expect("db-b status should exist");
        assert!(matches!(degraded.state, ClusterHostState::Degraded));
        assert!(
            degraded
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("pane launch failed"))
        );

        let binding = get_cluster_pane_binding(
            &runtime,
            ready
                .pane_id
                .as_deref()
                .expect("ready pane id should exist"),
        )
        .expect("binding lookup should succeed")
        .expect("binding should exist");
        assert_eq!(binding.state, ClusterConnectionState::Ready);
    }

    #[test]
    fn execute_cluster_up_continue_policy_allows_partial_launch_with_mixed_failures() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-precheck-fail", false);
        runtime.set_health("db-launch-fail", true);
        runtime.set_health("db-ok", true);
        runtime.fail_launch_for("db-launch-fail");

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([(
                "prod".to_string(),
                vec![
                    "db-precheck-fail".to_string(),
                    "db-launch-fail".to_string(),
                    "db-ok".to_string(),
                ],
            )]),
            known_targets: BTreeSet::from([
                "db-precheck-fail".to_string(),
                "db-launch-fail".to_string(),
                "db-ok".to_string(),
            ]),
        };

        let result = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Continue,
                retries: 0,
            },
        )
        .expect("continue policy should allow partial launch");

        let precheck_failed = result
            .statuses
            .iter()
            .find(|status| status.target == "db-precheck-fail")
            .expect("precheck-fail host status should exist");
        assert!(matches!(precheck_failed.state, ClusterHostState::Degraded));
        assert!(
            precheck_failed
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("probe exited with status"))
        );
        assert!(precheck_failed.pane_id.is_none());

        let launch_failed = result
            .statuses
            .iter()
            .find(|status| status.target == "db-launch-fail")
            .expect("launch-fail host status should exist");
        assert!(matches!(launch_failed.state, ClusterHostState::Degraded));
        assert!(
            launch_failed
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("pane launch failed"))
        );
        assert!(launch_failed.pane_id.is_none());

        let ready = result
            .statuses
            .iter()
            .find(|status| status.target == "db-ok")
            .expect("db-ok status should exist");
        assert!(matches!(ready.state, ClusterHostState::Ready));
        assert!(ready.pane_id.is_some());

        let panes = runtime
            .pane_list(&PaneListRequest { session: None })
            .expect("pane list should succeed")
            .panes;
        assert_eq!(panes.len(), 1, "only db-ok should have launched a pane");
    }

    #[test]
    fn execute_cluster_up_abort_policy_stops_on_launch_failure() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-precheck-fail", false);
        runtime.set_health("db-launch-fail", true);
        runtime.set_health("db-ok", true);
        runtime.fail_launch_for("db-launch-fail");

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([(
                "prod".to_string(),
                vec![
                    "db-precheck-fail".to_string(),
                    "db-launch-fail".to_string(),
                    "db-ok".to_string(),
                ],
            )]),
            known_targets: BTreeSet::from([
                "db-precheck-fail".to_string(),
                "db-launch-fail".to_string(),
                "db-ok".to_string(),
            ]),
        };

        let error = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Abort,
                retries: 0,
            },
        )
        .expect_err("abort policy should stop cluster-up on launch failure");
        assert!(error.contains("pane launch failed"));

        let panes = runtime
            .pane_list(&PaneListRequest { session: None })
            .expect("pane list should succeed")
            .panes;
        assert!(panes.is_empty(), "abort should stop before launching db-ok");
    }

    #[test]
    fn execute_cluster_up_prompt_policy_falls_back_to_abort_without_runtime() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-launch-fail", true);
        runtime.set_health("db-ok", true);
        runtime.fail_launch_for("db-launch-fail");

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([(
                "prod".to_string(),
                vec!["db-launch-fail".to_string(), "db-ok".to_string()],
            )]),
            known_targets: BTreeSet::from(["db-launch-fail".to_string(), "db-ok".to_string()]),
        };

        let error = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Prompt,
                retries: 0,
            },
        )
        .expect_err("prompt policy should abort when prompt runtime is unavailable");
        assert!(error.contains("pane launch failed"));

        let panes = runtime
            .pane_list(&PaneListRequest { session: None })
            .expect("pane list should succeed")
            .panes;
        assert!(
            panes.is_empty(),
            "prompt fallback abort should stop before launching db-ok"
        );
    }

    #[test]
    fn execute_cluster_up_abort_keeps_already_launched_panes() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-ok", true);
        runtime.set_health("db-launch-fail", true);
        runtime.set_health("db-after", true);
        runtime.fail_launch_for("db-launch-fail");

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([(
                "prod".to_string(),
                vec![
                    "db-ok".to_string(),
                    "db-launch-fail".to_string(),
                    "db-after".to_string(),
                ],
            )]),
            known_targets: BTreeSet::from([
                "db-ok".to_string(),
                "db-launch-fail".to_string(),
                "db-after".to_string(),
            ]),
        };

        let error = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Abort,
                retries: 0,
            },
        )
        .expect_err("abort should stop cluster-up on launch failure");
        assert!(error.contains("pane launch failed"));

        let panes = runtime
            .pane_list(&PaneListRequest { session: None })
            .expect("pane list should succeed")
            .panes;
        assert_eq!(panes.len(), 1, "already-launched panes should be kept");

        let first_pane_id = panes[0].id.to_string();
        let binding = get_cluster_pane_binding(&runtime, &first_pane_id)
            .expect("binding lookup should succeed")
            .expect("binding should exist");
        assert_eq!(binding.target, "db-ok");
    }

    #[test]
    fn execute_cluster_up_retries_post_launch_probe_until_ready() {
        let runtime = FakeRuntime::default();
        runtime.set_health_sequence("db-a", vec![true, false, true]);

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([("prod".to_string(), vec!["db-a".to_string()])]),
            known_targets: BTreeSet::from(["db-a".to_string()]),
        };

        let result = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Abort,
                retries: 1,
            },
        )
        .expect("post-launch retry should recover to ready");
        assert_eq!(result.statuses.len(), 1);
        assert!(matches!(result.statuses[0].state, ClusterHostState::Ready));

        let events = get_cluster_connection_events(&runtime).expect("event lookup should succeed");
        let target_events: Vec<&ClusterConnectionEvent> = events
            .iter()
            .filter(|event| event.target.as_deref() == Some("db-a"))
            .collect();
        assert!(
            target_events
                .iter()
                .any(|event| event.state == ClusterConnectionState::Retrying),
            "expected retrying event for db-a"
        );
        assert!(
            target_events
                .iter()
                .any(|event| event.state == ClusterConnectionState::Ready),
            "expected ready event for db-a"
        );
    }

    #[test]
    fn execute_cluster_pane_retry_falls_back_when_metadata_is_corrupt() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-a", true);
        let old_pane = runtime.add_pane(Some("host:db-a".to_string()), true);
        runtime
            .storage_set(&StorageSetRequest::new(
                pane_binding_storage_key(&old_pane.to_string()),
                vec![0xff, 0x00, 0x41],
            ))
            .expect("seed corrupt pane metadata should succeed");

        let result = execute_cluster_pane_retry(
            &runtime,
            &ClusterPaneRetryArgs {
                pane: PaneRetryRef::Active,
                on_failure: RetryFailurePolicy::Abort,
                retries: 0,
            },
        )
        .expect("retry should fall back to pane naming when metadata is corrupt");

        assert_eq!(result.target, "db-a");
        let new_binding = get_cluster_pane_binding(&runtime, &result.new_pane_id)
            .expect("new binding lookup should succeed")
            .expect("new binding should exist");
        assert_eq!(new_binding.state, ClusterConnectionState::Ready);
        assert_eq!(new_binding.source, "retry");
    }

    #[test]
    fn execute_cluster_pane_move_preserves_replacement_when_old_close_fails() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-a", true);
        runtime.set_health("db-b", true);
        let old_pane = runtime.add_pane(Some("prod:db-a".to_string()), true);
        set_cluster_pane_binding(
            &runtime,
            &old_pane.to_string(),
            Some(&ClusterPaneBinding {
                target: "db-a".to_string(),
                cluster: Some("prod".to_string()),
                source: "new".to_string(),
                state: ClusterConnectionState::Ready,
                retry_count: 0,
                last_error: None,
                updated_at_unix_ms: 1,
            }),
        )
        .expect("seed old pane binding should succeed");
        runtime.fail_close_for_pane(old_pane);

        let error = execute_cluster_pane_move(
            &runtime,
            ClusterPaneMoveArgs {
                pane: PaneRetryRef::Active,
                host: "db-b".to_string(),
            },
        )
        .expect_err("move should surface old pane close failure");
        assert!(error.contains("failed closing old pane"));

        let panes = runtime
            .pane_list(&PaneListRequest { session: None })
            .expect("pane list should succeed")
            .panes;
        assert_eq!(
            panes.len(),
            2,
            "replacement pane should still exist even when old close fails"
        );
        let replacement = panes
            .iter()
            .find(|pane| pane.id != old_pane)
            .expect("replacement pane should exist");

        let replacement_binding = get_cluster_pane_binding(&runtime, &replacement.id.to_string())
            .expect("replacement binding lookup should succeed")
            .expect("replacement binding should exist");
        assert_eq!(replacement_binding.target, "db-b");
    }

    #[test]
    fn append_cluster_connection_event_enforces_ring_buffer_limit() {
        let runtime = FakeRuntime::default();
        for index in 0..(CLUSTER_CONNECTION_EVENTS_MAX + 5) {
            append_cluster_connection_event(
                &runtime,
                ClusterConnectionEvent {
                    ts_unix_ms: u64::try_from(index).expect("index should fit u64"),
                    pane_id: Some(format!("p{index}")),
                    cluster: Some("prod".to_string()),
                    target: Some("db-a".to_string()),
                    source: Some("up".to_string()),
                    state: ClusterConnectionState::Connecting,
                    message: format!("event-{index}"),
                },
            )
            .expect("event append should succeed");
        }

        let events = get_cluster_connection_events(&runtime).expect("event lookup should succeed");
        assert_eq!(events.len(), CLUSTER_CONNECTION_EVENTS_MAX);
        assert_eq!(events[0].message, "event-5");
        assert_eq!(
            events[CLUSTER_CONNECTION_EVENTS_MAX - 1].message,
            format!("event-{}", CLUSTER_CONNECTION_EVENTS_MAX + 4)
        );
    }

    #[test]
    fn execute_cluster_pane_retry_replaces_pane_and_promotes_ready() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-a", true);
        let old_pane = runtime.add_pane(Some("host:db-a".to_string()), true);
        set_cluster_pane_binding(
            &runtime,
            &old_pane.to_string(),
            Some(&ClusterPaneBinding {
                target: "db-a".to_string(),
                cluster: None,
                source: "new".to_string(),
                state: ClusterConnectionState::Degraded,
                retry_count: 0,
                last_error: Some("simulated failure".to_string()),
                updated_at_unix_ms: 1,
            }),
        )
        .expect("seed binding should succeed");

        let result = execute_cluster_pane_retry(
            &runtime,
            &ClusterPaneRetryArgs {
                pane: PaneRetryRef::Active,
                on_failure: RetryFailurePolicy::Abort,
                retries: 0,
            },
        )
        .expect("retry should succeed");

        assert_eq!(result.target, "db-a");
        let old_pane_id = old_pane.to_string();
        assert_eq!(result.old_pane_id.as_deref(), Some(old_pane_id.as_str()));
        assert_ne!(
            result.new_pane_id, old_pane_id,
            "retry should create replacement pane"
        );

        let old_binding = get_cluster_pane_binding(&runtime, &old_pane.to_string())
            .expect("old binding lookup should succeed");
        assert!(old_binding.is_none(), "old pane binding should be cleared");

        let new_binding = get_cluster_pane_binding(&runtime, &result.new_pane_id)
            .expect("new binding lookup should succeed")
            .expect("new binding should exist");
        assert_eq!(new_binding.state, ClusterConnectionState::Ready);

        let panes = runtime
            .pane_list(&PaneListRequest { session: None })
            .expect("pane list should succeed")
            .panes;
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].id.to_string(), result.new_pane_id);
    }

    #[test]
    fn end_to_end_cluster_up_retry_and_events_flow_is_consistent() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-a", true);
        runtime.set_health_sequence("db-a", vec![true, false]);

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([("prod".to_string(), vec!["db-a".to_string()])]),
            known_targets: BTreeSet::from(["db-a".to_string()]),
        };

        let up = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Continue,
                retries: 0,
            },
        )
        .expect("cluster up should return partial success");
        assert_eq!(up.statuses.len(), 1);
        assert!(matches!(up.statuses[0].state, ClusterHostState::Degraded));
        let degraded_pane_id = up.statuses[0]
            .pane_id
            .clone()
            .expect("degraded launch should keep pane for retry");

        let retry = execute_cluster_pane_retry(
            &runtime,
            &ClusterPaneRetryArgs {
                pane: PaneRetryRef::Active,
                on_failure: RetryFailurePolicy::Abort,
                retries: 0,
            },
        )
        .expect("retry should recover pane to ready");
        assert_eq!(retry.target, "db-a");
        assert_eq!(
            retry.old_pane_id.as_deref(),
            Some(degraded_pane_id.as_str())
        );

        let events = get_cluster_connection_events(&runtime).expect("events should load");
        let target_events: Vec<&ClusterConnectionEvent> = events
            .iter()
            .filter(|event| event.target.as_deref() == Some("db-a"))
            .collect();
        assert!(
            target_events
                .iter()
                .any(|event| event.state == ClusterConnectionState::Connecting),
            "expected connecting event"
        );
        assert!(
            target_events
                .iter()
                .any(|event| event.state == ClusterConnectionState::Degraded),
            "expected degraded event"
        );
        assert!(
            target_events
                .iter()
                .any(|event| event.state == ClusterConnectionState::Retrying),
            "expected retrying event"
        );
        assert!(
            target_events
                .iter()
                .any(|event| event.state == ClusterConnectionState::Ready),
            "expected ready event"
        );

        let filtered_ready = filter_cluster_events(
            events,
            &ClusterEventsArgs {
                format: ClusterEventsFormat::Text,
                cluster: Some("prod".to_string()),
                target: Some("db-a".to_string()),
                state: Some(ClusterConnectionState::Ready),
                since_unix_ms: None,
                limit: Some(1),
            },
        );
        assert_eq!(filtered_ready.len(), 1);
        assert_eq!(filtered_ready[0].state, ClusterConnectionState::Ready);
    }

    #[test]
    fn end_to_end_cluster_up_abort_preserves_partial_state_and_event_tail() {
        let runtime = FakeRuntime::default();
        runtime.set_health("db-ok", true);
        runtime.set_health("db-fail", true);
        runtime.fail_launch_for("db-fail");

        let inventory = ClusterInventory {
            clusters: BTreeMap::from([(
                "prod".to_string(),
                vec![
                    "db-ok".to_string(),
                    "db-fail".to_string(),
                    "db-after".to_string(),
                ],
            )]),
            known_targets: BTreeSet::from([
                "db-ok".to_string(),
                "db-fail".to_string(),
                "db-after".to_string(),
            ]),
        };

        let error = execute_cluster_up(
            &runtime,
            &inventory,
            ClusterUpArgs {
                cluster: "prod".to_string(),
                hosts: Vec::new(),
                on_failure: RetryFailurePolicy::Abort,
                retries: 0,
            },
        )
        .expect_err("abort should stop on launch failure");
        assert!(error.contains("pane launch failed"));

        let panes = runtime
            .pane_list(&PaneListRequest { session: None })
            .expect("pane list should succeed")
            .panes;
        assert_eq!(panes.len(), 1, "already launched pane should remain");

        let events = get_cluster_connection_events(&runtime).expect("events should load");
        let filtered = filter_cluster_events(
            events,
            &ClusterEventsArgs {
                format: ClusterEventsFormat::Text,
                cluster: Some("prod".to_string()),
                target: None,
                state: None,
                since_unix_ms: None,
                limit: Some(1),
            },
        );
        assert_eq!(filtered.len(), 1);
        assert!(
            filtered[0].message.contains("pane launch failed")
                || filtered[0].state == ClusterConnectionState::Failed
        );
    }

    #[test]
    fn decide_failure_policy_action_non_prompt_modes_are_deterministic() {
        assert_eq!(
            decide_failure_policy_action(RetryFailurePolicy::Abort, "db-a", "boom"),
            RetryPromptDecision::Abort
        );
        assert_eq!(
            decide_failure_policy_action(RetryFailurePolicy::Continue, "db-a", "boom"),
            RetryPromptDecision::Continue
        );
    }

    #[test]
    fn parse_cluster_target_from_pane_name_extracts_suffix() {
        assert_eq!(
            parse_cluster_target_from_pane_name(Some("prod:db-a")).as_deref(),
            Some("db-a")
        );
        assert_eq!(
            parse_cluster_target_from_pane_name(Some("host:cache-b")).as_deref(),
            Some("cache-b")
        );
        assert_eq!(parse_cluster_target_from_pane_name(Some("invalid")), None);
    }

    #[test]
    fn parse_cluster_pane_move_args_supports_active_host_short_form() {
        let parsed =
            parse_cluster_pane_move_args(&["db-b".to_string()]).expect("move args should parse");
        assert!(matches!(parsed.pane, PaneRetryRef::Active));
        assert_eq!(parsed.host, "db-b");
    }

    #[test]
    fn parse_cluster_pane_move_args_supports_pane_and_host_positional() {
        let parsed = parse_cluster_pane_move_args(&["2".to_string(), "db-b".to_string()])
            .expect("move args should parse");
        assert!(matches!(parsed.pane, PaneRetryRef::Index(2)));
        assert_eq!(parsed.host, "db-b");
    }

    #[test]
    fn parse_cluster_pane_move_args_requires_host() {
        let error = parse_cluster_pane_move_args(&["--pane".to_string(), "2".to_string()])
            .expect_err("host should be required");
        assert!(error.contains("requires --host"));
    }

    #[test]
    fn retarget_pane_name_preserves_prefix() {
        assert_eq!(
            retarget_pane_name(Some("prod:db-a"), "db-b").as_deref(),
            Some("prod:db-b")
        );
        assert_eq!(
            retarget_pane_name(Some("host:cache-a"), "cache-b").as_deref(),
            Some("host:cache-b")
        );
    }

    #[test]
    fn parse_cluster_and_target_from_pane_name_handles_cluster_and_host_prefix() {
        assert_eq!(
            parse_cluster_and_target_from_pane_name(Some("prod:db-a")),
            Some((Some("prod".to_string()), "db-a".to_string()))
        );
        assert_eq!(
            parse_cluster_and_target_from_pane_name(Some("host:db-a")),
            Some((None, "db-a".to_string()))
        );
    }

    #[test]
    fn retarget_pane_name_with_cluster_prefers_cluster_metadata() {
        assert_eq!(
            retarget_pane_name_with_cluster(Some("host:cache-a"), Some("prod"), "db-b").as_deref(),
            Some("prod:db-b")
        );
    }
}
