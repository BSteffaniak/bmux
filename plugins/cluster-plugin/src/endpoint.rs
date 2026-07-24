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
