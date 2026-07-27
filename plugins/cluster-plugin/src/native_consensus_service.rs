use super::*;

pub fn invoke_native_consensus_service(context: &NativeServiceContext) -> Option<ServiceResponse> {
    let response = match (
        context.request.service.interface_id.as_str(),
        context.request.operation.as_str(),
    ) {
        ("cluster-control-command/v1", "mutate") => invoke_control_mutation(context),
        ("cluster-control-state/v1", "read_linearizable") => invoke_control_read(context, true),
        ("cluster-control-state/v1", "read_stale") => invoke_control_read(context, false),
        ("cluster-raft-rpc/v1", operation @ ("append_entries" | "install_snapshot" | "vote")) => {
            invoke_raft_rpc(context, operation)
        }
        _ => return None,
    };
    Some(response)
}

fn active_node(context: &NativeServiceContext) -> Result<consensus_runtime::ConsensusNode, String> {
    let identity = load_or_create_node_identity(context)?;
    consensus_network::global_consensus_nodes().get(*identity.node_id())
}

fn invoke_control_mutation(context: &NativeServiceContext) -> ServiceResponse {
    let request = match bmux_plugin_sdk::decode_service_message::<
        bmux_cluster_plugin_api::cluster_control_command::client::MutateRequest,
    >(&context.request.payload)
    {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_control_request", error.to_string()),
    };
    let result = active_node(context)
        .map_err(|reason| {
            bmux_cluster_plugin_api::cluster_types::ControlServiceError::RuntimeUnavailable {
                reason,
            }
        })
        .and_then(|node| {
            let handle = tokio::runtime::Handle::try_current().map_err(|error| {
                bmux_cluster_plugin_api::cluster_types::ControlServiceError::RuntimeUnavailable {
                    reason: error.to_string(),
                }
            })?;
            tokio::task::block_in_place(|| handle.block_on(node.mutate(request.request)))
        });
    typed_result(&result)
}

fn invoke_control_read(context: &NativeServiceContext, linearizable: bool) -> ServiceResponse {
    if let Err(error) = bmux_plugin_sdk::decode_service_message::<()>(&context.request.payload) {
        return ServiceResponse::error("invalid_control_request", error.to_string());
    }
    let result: Result<
        bmux_cluster_plugin_api::cluster_types::ControlStateView,
        bmux_cluster_plugin_api::cluster_types::ControlServiceError,
    > = (|| {
        let identity = load_or_create_node_identity(context).map_err(|reason| {
            bmux_cluster_plugin_api::cluster_types::ControlServiceError::RuntimeUnavailable {
                reason,
            }
        })?;
        let handle = consensus_network::ControlServiceHandle::new(
            Arc::new(context.clone()),
            *identity.node_id(),
            consensus_network::global_consensus_nodes(),
        );
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            bmux_cluster_plugin_api::cluster_types::ControlServiceError::RuntimeUnavailable {
                reason: error.to_string(),
            }
        })?;
        tokio::task::block_in_place(|| {
            if linearizable {
                runtime.block_on(
                    bmux_cluster_plugin_api::cluster_control_state::ClusterControlStateService::read_linearizable(
                        &handle,
                    ),
                )
            } else {
                runtime.block_on(
                    bmux_cluster_plugin_api::cluster_control_state::ClusterControlStateService::read_stale(
                        &handle,
                    ),
                )
            }
        })
    })();
    typed_result(&result)
}

fn invoke_raft_rpc(context: &NativeServiceContext, operation: &str) -> ServiceResponse {
    service_result((|| {
        let request = match bmux_plugin_sdk::decode_service_message::<
            bmux_cluster_plugin_api::cluster_raft_rpc::client::AppendEntriesRequest,
        >(&context.request.payload)
        {
            Ok(request) => request.request,
            Err(_) => bmux_plugin_sdk::decode_service_message::<
                bmux_cluster_plugin_api::cluster_types::RaftRpcRequest,
            >(&context.request.payload)
            .map_err(|error| error.to_string())?,
        };
        let identity = load_or_create_node_identity(context)?;
        let handle = consensus_network::RaftRpcServiceHandle::new(
            Arc::new(context.clone()),
            *identity.node_id(),
            consensus_network::global_consensus_nodes(),
        );
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| error.to_string())?;
        Ok(tokio::task::block_in_place(|| match operation {
            "append_entries" => runtime.block_on(
                bmux_cluster_plugin_api::cluster_raft_rpc::ClusterRaftRpcService::append_entries(
                    &handle, request,
                ),
            ),
            "install_snapshot" => runtime.block_on(
                bmux_cluster_plugin_api::cluster_raft_rpc::ClusterRaftRpcService::install_snapshot(
                    &handle, request,
                ),
            ),
            "vote" => runtime.block_on(
                bmux_cluster_plugin_api::cluster_raft_rpc::ClusterRaftRpcService::vote(
                    &handle, request,
                ),
            ),
            _ => unreachable!("closed raft RPC operation"),
        }))
    })())
}

fn typed_result<T: Serialize, E: Serialize>(result: &Result<T, E>) -> ServiceResponse {
    match bmux_plugin_sdk::encode_service_message(&result) {
        Ok(payload) => ServiceResponse::ok(payload),
        Err(error) => ServiceResponse::error("consensus_service_failed", error.to_string()),
    }
}

fn service_result<T: Serialize>(result: Result<T, String>) -> ServiceResponse {
    match result.and_then(|value| {
        bmux_plugin_sdk::encode_service_message(&value).map_err(|error| error.to_string())
    }) {
        Ok(payload) => ServiceResponse::ok(payload),
        Err(error) => ServiceResponse::error("consensus_service_failed", error),
    }
}
