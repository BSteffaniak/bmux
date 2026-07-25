use bmux_cluster_plugin_api::cluster_types::AuthenticatedPeer;
use bmux_connections_plugin_api::{
    connection_types::{InvocationOptions, ServiceInvocation, ServiceInvokeKind},
    connections_commands,
};
use bmux_plugin::{ServiceCaller, ServiceCallerDispatchClient};
use bmux_plugin_sdk::{TypedDispatchClient, TypedDispatchClientError};
use std::future::Future;

/// Generated typed-client transport bound to one explicit connection endpoint.
///
/// This stays private to the consuming plugin: the stable contracts remain the
/// generated plugin APIs, while `bmux.connections` owns endpoint transport.
#[derive(Debug)]
pub struct EndpointDispatchClient<'a, C: ServiceCaller + ?Sized> {
    caller: &'a C,
    endpoint: String,
    options: Option<InvocationOptions>,
}

impl<'a, C: ServiceCaller + ?Sized> EndpointDispatchClient<'a, C> {
    pub fn new(caller: &'a C, endpoint: impl Into<String>) -> Self {
        Self {
            caller,
            endpoint: endpoint.into(),
            options: None,
        }
    }

    #[cfg(test)]
    pub const fn with_options(mut self, options: InvocationOptions) -> Self {
        self.options = Some(options);
        self
    }
}

impl<C> TypedDispatchClient for EndpointDispatchClient<'_, C>
where
    C: ServiceCaller + Sync + ?Sized,
{
    fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: bmux_ipc::InvokeServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, TypedDispatchClientError>> + Send {
        let invocation = ServiceInvocation {
            target: self.endpoint.clone(),
            capability: capability.to_string(),
            kind: match kind {
                bmux_ipc::InvokeServiceKind::Query => ServiceInvokeKind::InvokeQuery,
                bmux_ipc::InvokeServiceKind::Command => ServiceInvokeKind::InvokeCommand,
            },
            interface_id: interface_id.to_string(),
            operation: operation.to_string(),
            payload,
            options: self.options.clone(),
        };
        let caller = self.caller;
        async move {
            let interface = invocation.interface_id.clone();
            let operation = invocation.operation.clone();
            connections_commands::client::invoke_service(
                &mut ServiceCallerDispatchClient::new(caller),
                invocation,
            )
            .await
            .map_err(|error| {
                TypedDispatchClientError::transport(
                    &interface,
                    &operation,
                    format!("connections service dispatch failed: {error}"),
                )
            })?
            .map_err(|error| {
                TypedDispatchClientError::transport(
                    interface,
                    operation,
                    format!("endpoint invocation failed: {error:?}"),
                )
            })
        }
    }
}

#[derive(Debug)]
pub enum PeerAuthenticationFailure {
    Unreachable(String),
    Untrusted(String),
    Local(String),
}

impl std::fmt::Display for PeerAuthenticationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(reason) | Self::Untrusted(reason) | Self::Local(reason) => {
                formatter.write_str(reason)
            }
        }
    }
}

pub async fn peer_authentication_proof<C>(
    caller: &C,
    endpoint: &str,
    local_node_id: &str,
    expected_remote_node_id: &str,
) -> Result<bmux_cluster_plugin_api::cluster_types::PeerAuthProof, PeerAuthenticationFailure>
where
    C: ServiceCaller + Sync + ?Sized,
{
    let mut remote = EndpointDispatchClient::new(caller, endpoint);
    let challenge = bmux_cluster_plugin_api::cluster_peer_auth::client::challenge(
        &mut remote,
        local_node_id.to_string(),
    )
    .await
    .map_err(|error| {
        PeerAuthenticationFailure::Unreachable(format!("remote peer challenge failed: {error}"))
    })?;
    if challenge.verifier_node_id != expected_remote_node_id {
        return Err(PeerAuthenticationFailure::Untrusted(
            "endpoint challenge was signed by an unexpected remote member".to_string(),
        ));
    }
    let mut local = ServiceCallerDispatchClient::new(caller);
    bmux_cluster_plugin_api::cluster_peer_auth::client::prove(&mut local, challenge)
        .await
        .map_err(|error| {
            PeerAuthenticationFailure::Local(format!("local peer proof failed: {error}"))
        })
}

pub async fn mutually_authenticate_endpoint<C>(
    caller: &C,
    endpoint: &str,
    local_node_id: &str,
    expected_remote_node_id: &str,
) -> Result<AuthenticatedPeer, PeerAuthenticationFailure>
where
    C: ServiceCaller + Sync + ?Sized,
{
    let proof =
        peer_authentication_proof(caller, endpoint, local_node_id, expected_remote_node_id).await?;
    let mut remote = EndpointDispatchClient::new(caller, endpoint);
    let peer = bmux_cluster_plugin_api::cluster_peer_auth::client::authenticate(&mut remote, proof)
        .await
        .map_err(|error| {
            PeerAuthenticationFailure::Untrusted(format!(
                "remote peer authentication failed: {error}"
            ))
        })?;
    if peer.node_id != local_node_id {
        return Err(PeerAuthenticationFailure::Untrusted(
            "remote verifier did not authenticate the local claimant".to_string(),
        ));
    }
    Ok(peer)
}

pub async fn probe_member_status<C>(
    caller: &C,
    local_node_id: &str,
    mut status: bmux_cluster_plugin_api::cluster_types::MemberStatus,
) -> bmux_cluster_plugin_api::cluster_types::MemberStatus
where
    C: ServiceCaller + Sync + ?Sized,
{
    use bmux_cluster_plugin_api::cluster_types::MemberLivenessState;

    if status.liveness != MemberLivenessState::Unchecked {
        return status;
    }
    let Some(endpoint) = status.member.endpoint.as_deref() else {
        return status;
    };
    match mutually_authenticate_endpoint(caller, endpoint, local_node_id, &status.member.node_id)
        .await
    {
        Ok(peer) if peer.node_id == local_node_id => {
            status.liveness = MemberLivenessState::Reachable;
            status.reachable = Some(true);
            status.authenticated_at_unix_ms = Some(peer.authenticated_at_unix_ms);
            status.reason = None;
        }
        Ok(_) => {
            status.liveness = MemberLivenessState::Untrusted;
            status.reachable = Some(true);
            status.trusted = false;
            status.reason = Some("endpoint authenticated as a different member".to_string());
        }
        Err(PeerAuthenticationFailure::Unreachable(error)) => {
            status.liveness = MemberLivenessState::Unreachable;
            status.reachable = Some(false);
            status.reason = Some(error);
        }
        Err(PeerAuthenticationFailure::Untrusted(error)) => {
            status.liveness = MemberLivenessState::Untrusted;
            status.reachable = Some(true);
            status.trusted = false;
            status.reason = Some(error);
        }
        Err(PeerAuthenticationFailure::Local(error)) => {
            status.liveness = MemberLivenessState::Untrusted;
            status.reachable = None;
            status.trusted = false;
            status.reason = Some(error);
        }
    }
    status.observed_at_unix_ms = crate::now_unix_ms();
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_cluster_plugin_api::{
        cluster_query,
        cluster_types::{ClusterHostState, ClusterHostStatus, ClusterStatusResult},
    };
    use bmux_plugin_sdk::{PluginError, Result as PluginResult, ServiceKind};
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct RecordingCaller {
        invocation: Mutex<Option<ServiceInvocation>>,
    }

    impl ServiceCaller for RecordingCaller {
        fn call_service_raw(
            &self,
            capability: &str,
            kind: ServiceKind,
            interface_id: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> PluginResult<Vec<u8>> {
            assert_eq!(capability, "bmux.connections.invoke");
            assert_eq!(kind, ServiceKind::Command);
            assert_eq!(interface_id, "connections-commands");
            assert_eq!(operation, "invoke-service");
            let request: connections_commands::client::InvokeServiceRequest =
                bmux_plugin_sdk::decode_service_message(&payload)?;
            *self.invocation.lock().expect("invocation lock poisoned") = Some(request.invocation);

            let endpoint_response = ClusterStatusResult {
                statuses: vec![ClusterHostStatus {
                    cluster: "synthetic".to_string(),
                    target: "worker-a".to_string(),
                    state: ClusterHostState::Ready,
                    reason: None,
                }],
            };
            let endpoint_payload = bmux_plugin_sdk::encode_service_message(&endpoint_response)?;
            bmux_plugin_sdk::encode_service_message(&Ok::<
                _,
                bmux_connections_plugin_api::connection_types::ConnectionError,
            >(endpoint_payload))
        }

        fn execute_kernel_request(
            &self,
            _request: bmux_ipc::Request,
        ) -> PluginResult<bmux_ipc::ResponsePayload> {
            Err(PluginError::UnsupportedHostOperation {
                operation: "execute_kernel_request",
            })
        }
    }

    #[tokio::test]
    async fn generated_client_routes_to_explicit_endpoint_through_connections_service() {
        let caller = RecordingCaller::default();
        let options = InvocationOptions {
            timeout_ms: 2_000,
            max_attempts: 2,
            retry_backoff_ms: 10,
        };
        let mut client =
            EndpointDispatchClient::new(&caller, "worker-a").with_options(options.clone());

        let response =
            cluster_query::client::status(&mut client, Some("synthetic".to_string()), Some(false))
                .await
                .expect("generated cluster query should route");
        assert_eq!(response.statuses.len(), 1);
        assert_eq!(response.statuses[0].target, "worker-a");

        let invocation = caller
            .invocation
            .lock()
            .expect("invocation lock poisoned")
            .clone()
            .expect("connections invocation should be recorded");
        assert_eq!(invocation.target, "worker-a");
        assert_eq!(invocation.capability, "bmux.server_clusters.read");
        assert_eq!(invocation.kind, ServiceInvokeKind::InvokeQuery);
        assert_eq!(invocation.interface_id, "cluster-query/v1");
        assert_eq!(invocation.operation, "status");
        assert_eq!(invocation.options, Some(options));
    }
}
