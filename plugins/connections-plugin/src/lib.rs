#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

mod admission;
mod config;
mod device_client;
mod directory;
mod identity;
mod listener;
mod outbound;
mod pairing;
mod storage;
mod target;
mod transport;

use bmux_client::{
    BmuxClient, ConnectionPoolAcquireError, ConnectionPoolLimits, EndpointConnectionLease,
    EndpointConnectionPool,
};
use bmux_connections_plugin_api::pairing_commands::client as pairing_requests;
use bmux_connections_plugin_api::{
    connection_types::{ConnectionError, InvocationOptions, ServiceInvokeKind},
    connections_commands::client::InvokeServiceRequest,
};
use bmux_plugin::stream_admission::StreamAdmissionAuthorityHandle;
use bmux_plugin_sdk::{CancellationToken, prelude::*};
use std::sync::{Arc, OnceLock};

pub use admission::AdmissionProviderCodec;
use bmux_plugin::stream_admission::StreamAdmissionAuthority;
pub use directory::LocalStatus;

#[derive(Default)]
pub struct ConnectionsPlugin {
    local_runtime: Option<directory::LocalRuntime>,
    pairing: Option<Arc<pairing::PairingAuthority>>,
    admission: Option<Arc<admission::AdmissionAuthority>>,
    admission_handle: Option<StreamAdmissionAuthorityHandle>,
    listener: tokio::sync::Mutex<Option<listener::Listener>>,
}

impl ConnectionsPlugin {
    /// Host integration must install this handle in its own registry before
    /// accepting admitted streams. This does not enable a listener.
    #[must_use]
    pub fn admission_handle(&self) -> Option<StreamAdmissionAuthorityHandle> {
        self.admission_handle.clone()
    }

    fn pairing(&self) -> Result<&pairing::PairingAuthority, ServiceResponse> {
        self.pairing
            .as_deref()
            .ok_or_else(|| ServiceResponse::error("unavailable", "pairing authority is inactive"))
    }

    fn admission(&self) -> Result<&admission::AdmissionAuthority, ServiceResponse> {
        self.admission
            .as_deref()
            .ok_or_else(|| ServiceResponse::error("unavailable", "admission authority is inactive"))
    }

    fn listener_status(
        &self,
    ) -> Result<bmux_connections_plugin_api::pairing_types::ListenerStatus, ServiceResponse> {
        let runtime = self
            .local_runtime
            .as_ref()
            .ok_or_else(|| ServiceResponse::error("unavailable", "inactive runtime"))?;
        let listener = self
            .listener
            .try_lock()
            .map_err(|_| ServiceResponse::error("busy", "listener transition in progress"))?;
        Ok(bmux_connections_plugin_api::pairing_types::ListenerStatus {
            endpoint_id: runtime.status().endpoint_id,
            enabled: listener.is_some(),
            endpoint_address: listener
                .as_ref()
                .and_then(|listener| listener.targets().into_iter().next()),
            endpoint_targets: listener
                .as_ref()
                .map_or_else(Vec::new, listener::Listener::targets),
        })
    }

    fn start_listener(
        &self,
        context: &NativeServiceContext,
    ) -> Result<bmux_connections_plugin_api::pairing_types::ListenerStatus, ServiceResponse> {
        let _owner = pairing_owner(context)?;
        let runtime = self
            .local_runtime
            .as_ref()
            .ok_or_else(|| ServiceResponse::error("unavailable", "inactive runtime"))?;
        if !runtime.ingress_allowed() {
            return Err(ServiceResponse::error(
                "ingress_denied",
                "explicit full-runtime ingress policy required",
            ));
        }
        // Auto-started listeners already retain their admission authority. An
        // idempotent owner start must not re-enter host service dispatch merely
        // to return status, nor retain the listener lock while reading status.
        {
            let listener = self
                .listener
                .try_lock()
                .map_err(|_| ServiceResponse::error("busy", "listener transition in progress"))?;
            if listener.is_some() {
                drop(listener);
                return self.listener_status();
            }
        }
        let caller = bmux_plugin::TypedServiceCaller::from_service_context(context);
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(&caller);
        let backend = bmux_plugin::block_on_typed_dispatch(
            bmux_clients_plugin_api::clients_identity::client::stream_admission_backend(
                &mut client,
            ),
        );
        let admission = self.admission()?;
        if !matches!(backend, Ok(Ok(Some(id))) if id == admission.instance()) {
            return Err(ServiceResponse::error(
                "backend_unconfirmed",
                "same-instance host admission registration required",
            ));
        }
        let pairing = Arc::clone(
            self.pairing
                .as_ref()
                .ok_or_else(|| ServiceResponse::error("unavailable", "inactive pairing"))?,
        );
        let admission = Arc::clone(
            self.admission
                .as_ref()
                .ok_or_else(|| ServiceResponse::error("unavailable", "inactive admission"))?,
        );
        let backend = bmux_ipc::IpcEndpoint::unix_socket(
            target::context_paths(&context.connection).server_socket(),
        );
        block_on_connection(async {
            let mut listener = self.listener.lock().await;
            if listener.is_none() {
                *listener = Some(
                    listener::Listener::start(
                        runtime.endpoint_builder().map_err(pairing_error)?,
                        pairing,
                        admission,
                        backend,
                    )
                    .await
                    .map_err(pairing_error)?,
                );
            }
            drop(listener);
            Ok::<_, ServiceResponse>(())
        })?;
        self.listener_status()
    }

    fn stop_listener(
        &self,
        context: &NativeServiceContext,
    ) -> Result<bmux_connections_plugin_api::pairing_types::ListenerStatus, ServiceResponse> {
        let _owner = pairing_owner(context)?;
        block_on_connection(async {
            let mut listener = self.listener.lock().await;
            if let Some(mut active) = listener.take() {
                active.stop().await;
            }
        });
        self.listener_status()
    }

    /// Local activation status without private key material or network activity.
    #[must_use]
    pub fn local_status(&self) -> Option<LocalStatus> {
        self.local_runtime
            .as_ref()
            .map(directory::LocalRuntime::status)
    }
}

impl RustPlugin for ConnectionsPlugin {
    type Contract = bmux_connections_plugin_api::Contract;

    fn register_contributions(
        &self,
        registrar: &mut bmux_plugin_sdk::ContributionRegistrar,
    ) -> bmux_plugin_sdk::Result<()> {
        use bmux_connections_plugin_api::pairing_state;
        use bmux_connections_plugin_api::{
            admission_provider_commands as commands, admission_provider_state as state,
        };
        use bmux_plugin::stream_admission::{
            STREAM_ADMISSION_PROVIDER_EXTENSION, StreamAdmissionProviderDeclaration,
            StreamAdmissionProviderOperation,
        };
        use bmux_plugin::stream_admission::{
            STREAM_ADMITTED_ROUTES_EXTENSION, StreamAdmittedRoute,
        };
        let operation = |interface: &str, operation: &str, contract: &[u8]| {
            Ok::<_, &str>(StreamAdmissionProviderOperation {
                capability: "bmux.connections.pairing".into(),
                interface_id: interface.into(),
                operation: operation.into(),
                contract: contract
                    .try_into()
                    .map_err(|_| "generated contract is not SHA-256")?,
            })
        };
        let invalid_contract =
            |details: &str| bmux_plugin_sdk::PluginError::InvalidNativeServiceOutput {
                plugin_id: "bmux.connections".into(),
                details: details.into(),
            };
        let declaration = StreamAdmissionProviderDeclaration {
            probe: operation(
                state::INTERFACE_ID.as_str(),
                state::OP_PROBE.as_str(),
                state::SERVICE_CONTRACT,
            )
            .map_err(invalid_contract)?,
            consume: operation(
                commands::INTERFACE_ID.as_str(),
                commands::OP_CONSUME.as_str(),
                commands::SERVICE_CONTRACT,
            )
            .map_err(invalid_contract)?,
            validate: operation(
                state::INTERFACE_ID.as_str(),
                state::OP_VALIDATE.as_str(),
                state::SERVICE_CONTRACT,
            )
            .map_err(invalid_contract)?,
            deadline_ms: 5_000,
            max_in_flight: 16,
        };
        registrar
            .register(bmux_plugin_sdk::PluginContribution::Extension {
                id: "connections.stream-admission-provider".into(),
                extension_point: STREAM_ADMISSION_PROVIDER_EXTENSION.into(),
                payload: bmux_plugin_sdk::encode_service_message(&declaration)?,
            })
            .map_err(
                |details| bmux_plugin_sdk::PluginError::InvalidNativeServiceOutput {
                    plugin_id: "bmux.connections".into(),
                    details,
                },
            )?;
        let routes: Vec<_> = [
            pairing_state::OP_CURRENT_PROVENANCE,
            pairing_state::OP_CHECK_DEVICE_ACCESS,
        ]
        .into_iter()
        .map(|operation| StreamAdmittedRoute {
            capability: "bmux.connections.pairing".into(),
            kind: bmux_plugin_sdk::ServiceKind::Query,
            interface_id: pairing_state::INTERFACE_ID.to_string(),
            operation: operation.to_string(),
        })
        .collect();
        registrar
            .register(bmux_plugin_sdk::PluginContribution::Extension {
                id: "connections.admitted-read-routes".into(),
                extension_point: STREAM_ADMITTED_ROUTES_EXTENSION.into(),
                payload: bmux_plugin_sdk::encode_service_message(&routes)?,
            })
            .map_err(
                |details| bmux_plugin_sdk::PluginError::InvalidNativeServiceOutput {
                    plugin_id: "bmux.connections".into(),
                    details,
                },
            )
    }

    fn activate(&mut self, context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        *self.listener.get_mut() = None;
        self.local_runtime = None;
        self.pairing = None;
        self.admission = None;
        self.admission_handle = None;
        let settings = config::RemoteSettings::load(context.settings.as_ref())
            .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        let identity = identity::RuntimeIdentity::load_or_create(
            std::path::Path::new(&context.connection.state_dir),
            std::path::Path::new(&context.connection.runtime_dir),
        )
        .map_err(|error| {
            PluginCommandError::failed(format!(
                "endpoint identity initialization failed: {error:#}"
            ))
        })?;
        let local_runtime = directory::LocalRuntime::new(identity, settings);
        let pairing = Arc::new(
            pairing::PairingAuthority::open(
                std::path::Path::new(&context.connection.state_dir),
                std::path::Path::new(&context.connection.runtime_dir),
                local_runtime.status().endpoint_id,
            )
            .map_err(|error| {
                PluginCommandError::failed(format!("pairing initialization failed: {error:#}"))
            })?,
        );
        let admission = Arc::new(admission::AdmissionAuthority::new(Arc::clone(&pairing)));
        self.admission_handle = Some(StreamAdmissionAuthorityHandle::new(admission.clone()));
        self.admission = Some(admission);
        self.pairing = Some(pairing);
        self.local_runtime = Some(local_runtime);
        if self
            .local_runtime
            .as_ref()
            .is_some_and(directory::LocalRuntime::auto_start)
        {
            let runtime = self.local_runtime.as_ref().expect("initialized runtime");
            let pairing = Arc::clone(self.pairing.as_ref().expect("initialized pairing"));
            let admission = Arc::clone(self.admission.as_ref().expect("initialized admission"));
            let backend = bmux_ipc::IpcEndpoint::unix_socket(
                target::context_paths(&context.connection).server_socket(),
            );
            // Local configuration explicitly enables this listener. Admission is
            // still enforced by the backend; no unadmitted fallback is possible.
            let builder = runtime
                .endpoint_builder()
                .map_err(|error| PluginCommandError::failed(error.to_string()))?;
            *self.listener.get_mut() = Some(
                block_on_connection(listener::Listener::start(
                    builder, pairing, admission, backend,
                ))
                .map_err(|error| {
                    PluginCommandError::failed(format!("listener startup failed: {error}"))
                })?,
            );
        }
        Ok(EXIT_OK)
    }

    fn deactivate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        *self.listener.get_mut() = None;
        self.local_runtime = None;
        self.pairing = None;
        self.admission = None;
        self.admission_handle = None;
        Ok(EXIT_OK)
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "pairing-state", "local-device-identity" => |(): (), ctx| {
                let _owner = pairing_owner(ctx)?;
                let paths = target::context_paths(&ctx.connection);
                identity::DeviceIdentity::load_or_create(&paths.state_dir, &paths.runtime_dir)
                    .map(|identity| identity.public_key()).map_err(pairing_error)
            },
            "pairing-commands", "pair-endpoint" => |request: pairing_requests::PairEndpointRequest, ctx| {
                let _owner = pairing_owner(ctx)?;
                let paths = target::context_paths(&ctx.connection);
                let config = target::load_config(&ctx.connection).map_err(pairing_error)?;
                let resolved = target::resolve_internal(&config, &request.target)
                    .map_err(|_| ServiceResponse::error("invalid_target", "device endpoint required"))?;
                let target::ResolvedTarget::Iroh(target) = resolved else {
                    return Err(ServiceResponse::error("invalid_target", "device endpoint required"));
                };
                block_on_connection(transport::pair_endpoint(&paths, &target, &request.invitation)).map_err(pairing_error)
            },
            "pairing-commands", "start-listener" => |(): (), ctx| { self.start_listener(ctx) },
            "pairing-commands", "stop-listener" => |(): (), ctx| { self.stop_listener(ctx) },
            "pairing-state", "listener-status" => |(): (), ctx| { let _owner = pairing_owner(ctx)?; self.listener_status() },
            "admission-provider-state", "probe" => |(): (), ctx| {
                host_internal(ctx)?;
                Ok::<_, ServiceResponse>(self.admission()?.instance())
            },
            "admission-provider-commands", "consume" => |request: bmux_connections_plugin_api::admission_provider_commands::client::ConsumeRequest, ctx| {
                host_internal(ctx)?;
                let binding = self.admission()?.consume(&request.ticket).map_err(|_| ServiceResponse::error("admission_denied", "invalid ticket"))?;
                Ok::<_, ServiceResponse>(bmux_connections_plugin_api::pairing_types::AdmissionBinding { subject_id: binding.subject_id(), lease_id: binding.lease_id() })
            },
            "admission-provider-state", "validate" => |request: bmux_connections_plugin_api::admission_provider_state::client::ValidateRequest, ctx| {
                host_internal(ctx)?;
                let binding = bmux_plugin::stream_admission::StreamAdmissionBinding::new(request.subject_id, request.lease_id);
                Ok::<_, ServiceResponse>(self.admission()?.validate(&binding).is_ok())
            },
            "pairing-commands", "create-invitation" => |request: pairing_requests::CreateInvitationRequest, ctx| {
                let owner = pairing_owner(ctx)?;
                self.pairing()?.create_invitation(&owner, &request.device_key, request.ttl_seconds, request.access).map_err(pairing_error)
            },
            "pairing-commands", "redeem-invitation" => |request: pairing_requests::RedeemInvitationRequest, _ctx| {
                self.pairing()?.redeem(&request.invitation_id, &request.secret, &request.signature).map_err(pairing_error)
            },
            "pairing-commands", "grant-device" => |request: pairing_requests::GrantDeviceRequest, ctx| {
                let owner = pairing_owner(ctx)?;
                self.pairing()?.grant(&owner, &request.device_id, request.access).map_err(pairing_error)
            },
            "pairing-commands", "revoke-device" => |request: pairing_requests::RevokeDeviceRequest, ctx| {
                let owner = pairing_owner(ctx)?;
                self.pairing()?.revoke(&owner, &request.device_id).map_err(pairing_error)
            },
            "pairing-commands", "begin-admission" => |request: pairing_requests::BeginAdmissionRequest, _ctx| {
                self.admission()?.begin(&request.device_id).map_err(pairing_error)
            },
            "pairing-commands", "finish-admission" => |request: pairing_requests::FinishAdmissionRequest, _ctx| {
                self.admission()?.finish(&request.challenge, &request.signature).map_err(pairing_error)
            },
            "pairing-state", "current-provenance" => |(): (), ctx| {
                Ok::<_, ServiceResponse>(current_provenance(ctx))
            },
            "pairing-state", "list-devices" => |(): (), ctx| {
                let owner = pairing_owner(ctx)?;
                self.pairing()?.list_devices(&owner).map_err(pairing_error)
            },
            "pairing-state", "check-device-access" => |request: bmux_connections_plugin_api::pairing_state::client::CheckDeviceAccessRequest, _ctx| {
                self.pairing()?.check_device_access(request.subject_id).map_err(pairing_error)
            },
            "connections-state", "resolve" => |request: bmux_connections_plugin_api::connections_state::client::ResolveRequest, ctx| {
                let config = target::load_config(&ctx.connection)
                    .map_err(|error| ServiceResponse::error("config_failed", error.to_string()))?;
                let paths = target::context_paths(&ctx.connection);
                let target = block_on_connection(async { target::expand_reference(&config, &paths, &request.target).await })
                    .map_err(|error| ServiceResponse::error("resolution_failed", format!("{error:?}")))?;
                Ok::<_, ServiceResponse>(target::resolve(&config, &target))
            },
            "connections-state", "list-targets" => |(): (), ctx| {
                let config = target::load_config(&ctx.connection)
                    .map_err(|error| ServiceResponse::error("config_failed", error.to_string()))?;
                Ok::<_, ServiceResponse>(target::list(&config))
            },
            "connections-commands", "invoke-service" => |request: InvokeServiceRequest, ctx| {
                let config = target::load_config(&ctx.connection)
                    .map_err(|error| ServiceResponse::error("config_failed", error.to_string()))?;
                let paths = target::context_paths(&ctx.connection);
                Ok::<Result<Vec<u8>, ConnectionError>, ServiceResponse>(invoke_service(
                    &config,
                    &paths,
                    &ctx.cancellation,
                    request,
                ))
            },
        })
    }
}

fn pairing_error(_error: anyhow::Error) -> ServiceResponse {
    // No secret-bearing parser/crypto details cross the service boundary.
    ServiceResponse::error("pairing_denied", "pairing operation failed")
}

fn current_provenance(
    context: &NativeServiceContext,
) -> bmux_connections_plugin_api::pairing_types::StreamProvenance {
    use bmux_clients_plugin_api::clients_identity::StreamProvenance as Origin;
    use bmux_connections_plugin_api::pairing_types::StreamProvenance as Output;
    let caller = bmux_plugin::TypedServiceCaller::from_service_context(context);
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(&caller);
    match bmux_plugin::block_on_typed_dispatch(
        bmux_clients_plugin_api::clients_identity::client::current_stream_provenance(&mut client),
    ) {
        Ok(Ok(Origin::Local)) => Output::Local,
        Ok(Ok(Origin::Admitted {
            subject_id,
            lease_id,
        })) => Output::Admitted {
            subject_id,
            lease_id,
        },
        _ => Output::Invalid,
    }
}

fn host_internal(context: &NativeServiceContext) -> Result<(), ServiceResponse> {
    let caller = bmux_plugin::TypedServiceCaller::from_service_context(context);
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(&caller);
    if matches!(
        bmux_plugin::block_on_typed_dispatch(
            bmux_clients_plugin_api::clients_identity::client::current_stream_provenance(
                &mut client
            )
        ),
        Ok(Ok(
            bmux_clients_plugin_api::clients_identity::StreamProvenance::HostInternal
        ))
    ) {
        Ok(())
    } else {
        Err(ServiceResponse::error(
            "host_required",
            "host-internal provider invocation required",
        ))
    }
}

fn pairing_owner(context: &NativeServiceContext) -> Result<pairing::LocalOwner, ServiceResponse> {
    if current_provenance(context)
        == bmux_connections_plugin_api::pairing_types::StreamProvenance::Local
    {
        Ok(pairing::LocalOwner::from_trusted_local_owner())
    } else {
        Err(ServiceResponse::error(
            "owner_required",
            "trusted local owner provenance required",
        ))
    }
}

fn invoke_service(
    config: &bmux_config::BmuxConfig,
    paths: &bmux_config::ConfigPaths,
    cancellation: &CancellationToken,
    request: InvokeServiceRequest,
) -> Result<Vec<u8>, ConnectionError> {
    let invocation = request.invocation;
    let target_name = invocation.target.clone();
    let options = normalized_invocation_options(invocation.options.as_ref());
    if cancellation.is_cancelled() {
        return Err(ConnectionError::Cancelled {
            target: target_name,
        });
    }
    let timeout_ms = cancellation
        .deadline_ms
        .map_or(options.timeout_ms, |deadline| {
            deadline.min(options.timeout_ms)
        });
    if timeout_ms == 0 {
        return Err(ConnectionError::TimedOut {
            target: target_name,
            phase: "request".to_string(),
            timeout_ms,
        });
    }

    block_on_connection(async move {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let remaining =
            remaining_before_deadline(deadline).ok_or_else(|| ConnectionError::TimedOut {
                target: invocation.target.clone(),
                phase: "resolve".to_string(),
                timeout_ms,
            })?;
        let expanded = tokio::time::timeout(
            remaining,
            target::expand_reference(config, paths, &invocation.target),
        )
        .await
        .map_err(|_| ConnectionError::TimedOut {
            target: invocation.target.clone(),
            phase: "resolve".to_string(),
            timeout_ms,
        })??;
        let resolved = target::resolve_internal(config, &expanded)?;
        let kind = match invocation.kind {
            ServiceInvokeKind::InvokeQuery => bmux_ipc::InvokeServiceKind::Query,
            ServiceInvokeKind::InvokeCommand => bmux_ipc::InvokeServiceKind::Command,
        };
        let mut client = acquire_endpoint(
            config,
            paths,
            &resolved,
            AcquisitionPolicy {
                target: &invocation.target,
                cancellation,
                options: &options,
                deadline,
                timeout_ms,
            },
        )
        .await?;
        let remaining =
            remaining_before_deadline(deadline).ok_or_else(|| ConnectionError::TimedOut {
                target: invocation.target.clone(),
                phase: "invoke".to_string(),
                timeout_ms,
            })?;
        // Service calls are never replayed: a timeout or disconnect after
        // dispatch has an ambiguous completion state, especially for
        // non-idempotent commands. Ambiguous or failed clients are discarded
        // instead of returning to the idle pool.
        // Connection setup and each service invocation have independent budgets.
        // A pooled client must not retain another invocation's request timeout.
        let previous_timeout = client.set_request_timeout(remaining);
        let response = tokio::time::timeout(
            remaining,
            client.invoke_service_raw(
                &invocation.capability,
                kind,
                &invocation.interface_id,
                &invocation.operation,
                invocation.payload,
            ),
        )
        .await;
        client.set_request_timeout(previous_timeout);
        match response {
            Ok(Ok(payload)) => Ok(payload),
            Ok(Err(error)) => {
                client.mark_unhealthy();
                Err(ConnectionError::ServiceFailed {
                    target: invocation.target,
                    reason: error.to_string(),
                })
            }
            Err(_) => {
                client.mark_unhealthy();
                Err(ConnectionError::TimedOut {
                    target: invocation.target,
                    phase: "invoke".to_string(),
                    timeout_ms,
                })
            }
        }
    })
}

struct AcquisitionPolicy<'a> {
    target: &'a str,
    cancellation: &'a CancellationToken,
    options: &'a InvocationOptions,
    deadline: tokio::time::Instant,
    timeout_ms: u64,
}

fn connection_pool() -> &'static EndpointConnectionPool<BmuxClient> {
    static POOL: OnceLock<EndpointConnectionPool<BmuxClient>> = OnceLock::new();
    POOL.get_or_init(|| {
        EndpointConnectionPool::new(ConnectionPoolLimits::default())
            .expect("default connection pool limits are valid")
    })
}

async fn acquire_endpoint(
    config: &bmux_config::BmuxConfig,
    paths: &bmux_config::ConfigPaths,
    resolved: &target::ResolvedTarget,
    policy: AcquisitionPolicy<'_>,
) -> Result<EndpointConnectionLease<BmuxClient>, ConnectionError> {
    let AcquisitionPolicy {
        target,
        cancellation,
        options,
        deadline,
        timeout_ms,
    } = policy;
    let endpoint_id = resolved.connection_identity(config, paths);
    let mut last_connection_error = None;
    for attempt in 1..=options.max_attempts {
        if cancellation.is_cancelled() {
            return Err(ConnectionError::Cancelled {
                target: target.to_string(),
            });
        }
        let resolved = resolved.clone();
        match connection_pool()
            .acquire(endpoint_id.clone(), deadline, || async move {
                transport::connect(config, paths, &resolved).await
            })
            .await
        {
            Ok(client) => return Ok(client),
            Err(ConnectionPoolAcquireError::Connect(
                error @ ConnectionError::ConnectionFailed { .. },
            )) => {
                if options.max_attempts == 1 {
                    return Err(error);
                }
                last_connection_error = Some(error);
            }
            Err(ConnectionPoolAcquireError::Connect(error)) => return Err(error),
            Err(ConnectionPoolAcquireError::AdmissionTimedOut) => {
                let error = ConnectionError::TimedOut {
                    target: target.to_string(),
                    phase: "pool-admission".to_string(),
                    timeout_ms,
                };
                if options.max_attempts == 1 {
                    return Err(error);
                }
                last_connection_error = Some(error);
            }
        }
        if attempt < options.max_attempts {
            wait_for_retry(
                cancellation,
                deadline,
                options.retry_backoff_ms,
                target,
                timeout_ms,
            )
            .await?;
        }
    }
    Err(ConnectionError::RetryExhausted {
        target: target.to_string(),
        attempts: options.max_attempts,
        reason: last_connection_error.map_or_else(
            || "endpoint acquisition failed".to_string(),
            |error| format!("{error:?}"),
        ),
    })
}

const DEFAULT_INVOCATION_TIMEOUT_MS: u64 = 30_000;
const MAX_INVOCATION_ATTEMPTS: u32 = 10;
const MAX_RETRY_BACKOFF_MS: u64 = 30_000;

fn normalized_invocation_options(options: Option<&InvocationOptions>) -> InvocationOptions {
    let options = options.cloned().unwrap_or(InvocationOptions {
        timeout_ms: DEFAULT_INVOCATION_TIMEOUT_MS,
        max_attempts: 1,
        retry_backoff_ms: 0,
    });
    InvocationOptions {
        timeout_ms: options.timeout_ms,
        max_attempts: options.max_attempts.clamp(1, MAX_INVOCATION_ATTEMPTS),
        retry_backoff_ms: options.retry_backoff_ms.min(MAX_RETRY_BACKOFF_MS),
    }
}

fn remaining_before_deadline(deadline: tokio::time::Instant) -> Option<std::time::Duration> {
    deadline.checked_duration_since(tokio::time::Instant::now())
}

async fn wait_for_retry(
    cancellation: &CancellationToken,
    deadline: tokio::time::Instant,
    backoff_ms: u64,
    target: &str,
    timeout_ms: u64,
) -> Result<(), ConnectionError> {
    if cancellation.is_cancelled() {
        return Err(ConnectionError::Cancelled {
            target: target.to_string(),
        });
    }
    let remaining =
        remaining_before_deadline(deadline).ok_or_else(|| ConnectionError::TimedOut {
            target: target.to_string(),
            phase: "retry-backoff".to_string(),
            timeout_ms,
        })?;
    if backoff_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms).min(remaining)).await;
    }
    if cancellation.is_cancelled() {
        return Err(ConnectionError::Cancelled {
            target: target.to_string(),
        });
    }
    if remaining_before_deadline(deadline).is_none() {
        return Err(ConnectionError::TimedOut {
            target: target.to_string(),
            phase: "retry-backoff".to_string(),
            timeout_ms,
        });
    }
    Ok(())
}

fn block_on_connection<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T> + Send,
    T: Send,
{
    let handle = tokio::runtime::Handle::try_current()
        .expect("connections service requires the host tokio runtime");
    tokio::task::block_in_place(|| handle.block_on(future))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_config::{BmuxConfig, ConfigPaths};
    use bmux_connections_plugin_api::connection_types::ServiceInvokeKind;
    use bmux_plugin_sdk::{
        CancellationToken, HostConnectionInfo, HostMetadata, HostScope, NativeServiceContext,
        ProviderId, RegisteredService, ServiceKind, ServiceRequest,
    };
    use bmux_server::BmuxServer;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn test_paths(root: &std::path::Path) -> ConfigPaths {
        ConfigPaths::new(
            root.join("config"),
            root.join("runtime"),
            root.join("data"),
            root.join("state"),
        )
    }

    #[test]
    fn activation_loads_identity_and_settings_without_ingress() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let runtime = root.path().join("runtime");
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir(&runtime).unwrap();
        let context: NativeLifecycleContext = serde_json::from_value(serde_json::json!({
            "plugin_id": "bmux.connections",
            "host": { "product_name": "bmux", "product_version": "test", "plugin_api_version": bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION, "plugin_abi_version": bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION },
            "connection": { "config_dir": root.path(), "runtime_dir": runtime, "data_dir": root.path(), "state_dir": state },
            "settings": serde_json::to_string(&toml::from_str::<toml::Value>("[remote]\nstartup_wait_ms = 1234\n[remote.relay]\nmode = 'disabled'").unwrap()).unwrap()
        })).unwrap();
        let mut plugin = ConnectionsPlugin::default();
        plugin.activate(context.clone()).unwrap();
        let first = plugin.local_status().unwrap();
        assert_eq!(first.relay_mode, "disabled");
        assert_eq!(first.startup_wait_ms, 1234);
        assert!(!first.listener_enabled);
        plugin.deactivate(context.clone()).unwrap();
        assert!(plugin.local_status().is_none());
        plugin.activate(context).unwrap();
        assert_eq!(plugin.local_status().unwrap(), first);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn configured_listener_restarts_at_stable_public_endpoint() {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        drop(socket);
        let settings = format!(
            "[remote]\nallow_full_runtime_ingress = true\nauto_start = true\nbind_addr = '{address}'\n[remote.relay]\nmode = 'disabled'"
        );
        let context: NativeLifecycleContext = serde_json::from_value(serde_json::json!({
            "plugin_id": "bmux.connections",
            "host": { "product_name": "bmux", "product_version": "test", "plugin_api_version": bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION, "plugin_abi_version": bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION },
            "connection": { "config_dir": paths.config_dir, "runtime_dir": paths.runtime_dir, "data_dir": paths.data_dir, "state_dir": paths.state_dir },
            "settings": serde_json::to_string(&toml::from_str::<toml::Value>(&settings).unwrap()).unwrap()
        })).unwrap();
        let mut plugin = ConnectionsPlugin::default();
        plugin.activate(context.clone()).unwrap();
        let first = plugin.listener_status().unwrap();
        assert!(first.enabled);
        let _router = trusted_local_router();
        let owner_start = service_context(&paths, "start-listener", Vec::new());
        // Router accepts only provenance queries: redundant backend callbacks
        // fail this test rather than hiding idempotency behind a permissive stub.
        for _ in 0..2 {
            assert_eq!(
                plugin
                    .start_listener(&owner_start)
                    .unwrap()
                    .endpoint_targets,
                first.endpoint_targets
            );
        }
        assert!(
            first
                .endpoint_targets
                .contains(&format!("device://{}?addr={address}", first.endpoint_id))
        );
        plugin.deactivate(context.clone()).unwrap();
        plugin.activate(context.clone()).unwrap();
        assert_eq!(
            plugin
                .start_listener(&owner_start)
                .unwrap()
                .endpoint_targets,
            first.endpoint_targets
        );
        assert_eq!(
            plugin.listener_status().unwrap().endpoint_targets,
            first.endpoint_targets
        );
        plugin.deactivate(context).unwrap();
    }

    #[test]
    fn outbound_owner_api_rejects_admitted_origin() {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        let _router = provenance_router(
            bmux_clients_plugin_api::clients_identity::StreamProvenance::Admitted {
                subject_id: uuid::Uuid::new_v4(),
                lease_id: uuid::Uuid::new_v4(),
            },
        );
        let plugin = ConnectionsPlugin::default();
        let mut context = service_context(
            &paths,
            "local-device-identity",
            bmux_plugin_sdk::encode_service_message(&()).unwrap(),
        );
        context.request.service.interface_id = "pairing-state".into();
        assert_eq!(
            plugin.invoke_service(context).error.unwrap().code,
            "owner_required"
        );
        let invitation = bmux_connections_plugin_api::pairing_types::Invitation {
            invitation_id: String::new(),
            endpoint_id: String::new(),
            device_key: String::new(),
            secret: Vec::new(),
            expires_at: 0,
            access: bmux_connections_plugin_api::pairing_types::DeviceAccess::Denied,
            possession_message: Vec::new(),
        };
        let mut context = service_context(
            &paths,
            "pair-endpoint",
            bmux_plugin_sdk::encode_service_message(&pairing_requests::PairEndpointRequest {
                target: "device://invalid".into(),
                invitation,
            })
            .unwrap(),
        );
        context.request.service.interface_id = "pairing-commands".into();
        assert_eq!(
            plugin.invoke_service(context).error.unwrap().code,
            "owner_required"
        );
    }

    fn service_context(
        paths: &ConfigPaths,
        operation: &str,
        payload: Vec<u8>,
    ) -> NativeServiceContext {
        NativeServiceContext {
            plugin_id: "bmux.connections".to_string(),
            request: ServiceRequest {
                caller_plugin_id: "test.caller".to_string(),
                service: RegisteredService {
                    capability: HostScope::new(if operation == "invoke-service" {
                        "bmux.connections.invoke"
                    } else {
                        "bmux.connections.read"
                    })
                    .expect("capability"),
                    kind: if operation == "invoke-service" {
                        ServiceKind::Command
                    } else {
                        ServiceKind::Query
                    },
                    interface_id: if operation == "invoke-service" {
                        "connections-commands".to_string()
                    } else {
                        "connections-state".to_string()
                    },
                    provider: ProviderId::Plugin("bmux.connections".to_string()),
                },
                operation: operation.to_string(),
                payload,
            },
            required_capabilities: Vec::new(),
            provided_capabilities: vec![
                "bmux.connections.read".to_string(),
                "bmux.connections.invoke".to_string(),
            ],
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: vec!["bmux.connections".to_string()],
            plugin_search_roots: Vec::new(),
            host: HostMetadata {
                product_name: "bmux-test".to_string(),
                product_version: "0.0.0".to_string(),
                plugin_api_version: bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION,
                plugin_abi_version: bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION,
            },
            connection: HostConnectionInfo {
                config_dir: paths.config_dir.to_string_lossy().into_owned(),
                config_dir_candidates: Vec::new(),
                runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
                data_dir: paths.data_dir.to_string_lossy().into_owned(),
                state_dir: paths.state_dir.to_string_lossy().into_owned(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            cancellation: CancellationToken::new(),
            host_kernel_bridge: None,
        }
    }

    fn trusted_local_router() -> bmux_plugin::test_support::TestServiceRouterGuard {
        provenance_router(bmux_clients_plugin_api::clients_identity::StreamProvenance::Local)
    }

    fn provenance_router(
        origin: bmux_clients_plugin_api::clients_identity::StreamProvenance,
    ) -> bmux_plugin::test_support::TestServiceRouterGuard {
        bmux_plugin::test_support::install_test_service_router(Arc::new(
            // The required test router signature uses PluginError by value.
            #[allow(clippy::result_large_err)]
            move |_, _, _, _, interface, operation, payload| {
                bmux_plugin_sdk::service_contract::decode_for(
                    interface,
                    bmux_clients_plugin_api::clients_identity::SERVICE_CONTRACT,
                    &payload,
                )?;
                assert_eq!(
                    interface,
                    bmux_clients_plugin_api::clients_identity::INTERFACE_ID.as_str()
                );
                assert_eq!(operation, "current-stream-provenance");
                let response =
                    bmux_plugin_sdk::encode_service_message(&Ok::<
                        _,
                        bmux_clients_plugin_api::clients_identity::IdentityError,
                    >(origin.clone()))?;
                Ok(bmux_plugin_sdk::service_contract::encode_response(
                    &response,
                ))
            },
        ))
        // The router remains installed only for this test scope.
    }

    #[test]
    fn activation_contribution_matches_generated_provider_contracts() {
        use bmux_connections_plugin_api::{
            admission_provider_commands as commands, admission_provider_state as state,
        };
        use bmux_plugin::stream_admission::{
            STREAM_ADMISSION_PROVIDER_EXTENSION, StreamAdmissionProviderCodec,
            StreamAdmissionProviderDeclaration,
        };
        let mut registrar = bmux_plugin_sdk::ContributionRegistrar::new();
        ConnectionsPlugin::default()
            .register_contributions(&mut registrar)
            .unwrap();
        let contributions = registrar.into_contributions();
        let [
            bmux_plugin_sdk::PluginContribution::Extension {
                extension_point,
                payload,
                ..
            },
            bmux_plugin_sdk::PluginContribution::Extension {
                extension_point: routes_point,
                payload: routes_payload,
                ..
            },
        ] = contributions.as_slice()
        else {
            panic!("expected provider extension")
        };
        assert_eq!(extension_point, STREAM_ADMISSION_PROVIDER_EXTENSION);
        assert_eq!(
            routes_point,
            bmux_plugin::stream_admission::STREAM_ADMITTED_ROUTES_EXTENSION
        );
        let routes: Vec<bmux_plugin::stream_admission::StreamAdmittedRoute> =
            bmux_plugin_sdk::decode_service_message(routes_payload).unwrap();
        assert_eq!(routes.len(), 2);
        assert!(
            routes
                .iter()
                .all(|route| route.kind == bmux_plugin_sdk::ServiceKind::Query
                    && route.interface_id == "pairing-state"
                    && matches!(
                        route.operation.as_str(),
                        "current-provenance" | "check-device-access"
                    ))
        );
        let declaration: StreamAdmissionProviderDeclaration =
            bmux_plugin_sdk::decode_service_message(payload).unwrap();
        assert_eq!(declaration.probe.interface_id, state::INTERFACE_ID.as_str());
        assert_eq!(
            declaration.consume.interface_id,
            commands::INTERFACE_ID.as_str()
        );
        assert_eq!(
            declaration.probe.contract.as_slice(),
            state::SERVICE_CONTRACT
        );
        assert_eq!(
            declaration.consume.contract.as_slice(),
            commands::SERVICE_CONTRACT
        );
        assert_eq!(declaration.validate.operation, "validate");
        assert_eq!(declaration.deadline_ms, 5_000);
        assert_eq!(declaration.max_in_flight, 16);
        let codec = AdmissionProviderCodec;
        assert_eq!(
            declaration.encode_probe().unwrap(),
            codec.encode_probe().unwrap()
        );
        assert_eq!(
            declaration.encode_consume(&[9; 32]).unwrap(),
            codec.encode_consume(&[9; 32]).unwrap()
        );
        let binding = bmux_plugin::stream_admission::StreamAdmissionBinding::new(
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
        );
        assert_eq!(
            declaration.encode_validate(&binding).unwrap(),
            codec.encode_validate(&binding).unwrap()
        );
    }

    #[test]
    fn listener_start_requires_owner_policy_and_same_backend_instance() {
        use bmux_clients_plugin_api::clients_identity::StreamProvenance as Origin;
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        let identity =
            identity::RuntimeIdentity::load_or_create(&paths.state_dir, &paths.runtime_dir)
                .unwrap();
        let pairing = Arc::new(
            pairing::PairingAuthority::open(
                &paths.state_dir,
                &paths.runtime_dir,
                identity.endpoint_id(),
            )
            .unwrap(),
        );
        let admission = Arc::new(admission::AdmissionAuthority::new(pairing));
        let mut plugin = ConnectionsPlugin {
            local_runtime: Some(directory::LocalRuntime::new(
                identity,
                config::RemoteSettings::default(),
            )),
            admission: Some(admission),
            ..ConnectionsPlugin::default()
        };
        let context = service_context(&paths, "start-listener", Vec::new());
        assert!(plugin.start_listener(&context).is_err());
        {
            let _router = provenance_router(Origin::Admitted {
                subject_id: uuid::Uuid::new_v4(),
                lease_id: uuid::Uuid::new_v4(),
            });
            assert!(plugin.start_listener(&context).is_err());
        }
        {
            let _router = trusted_local_router();
            let error = plugin.start_listener(&context).unwrap_err();
            assert_eq!(error.error.unwrap().code, "ingress_denied");
        }
        let identity =
            identity::RuntimeIdentity::load_or_create(&paths.state_dir, &paths.runtime_dir)
                .unwrap();
        let settings = config::RemoteSettings {
            allow_full_runtime_ingress: true,
            ..config::RemoteSettings::default()
        };
        plugin.local_runtime = Some(directory::LocalRuntime::new(identity, settings));
        let _router = bmux_plugin::test_support::install_test_service_router(Arc::new(
            // The required test router signature uses PluginError by value.
            #[allow(clippy::result_large_err)]
            |_, _, _, _, interface, operation, payload| {
                bmux_plugin_sdk::service_contract::decode_for(
                    interface,
                    bmux_clients_plugin_api::clients_identity::SERVICE_CONTRACT,
                    &payload,
                )?;
                let response = match operation {
                    "current-stream-provenance" => Ok::<_, bmux_plugin_sdk::PluginError>(
                        bmux_plugin_sdk::encode_service_message(&Ok::<
                            _,
                            bmux_clients_plugin_api::clients_identity::IdentityError,
                        >(
                            Origin::Local
                        ))
                        .unwrap(),
                    ),
                    "stream-admission-backend" => Ok::<_, bmux_plugin_sdk::PluginError>(
                        bmux_plugin_sdk::encode_service_message(&Ok::<
                            _,
                            bmux_clients_plugin_api::clients_identity::IdentityError,
                        >(Some(
                            uuid::Uuid::new_v4(),
                        )))
                        .unwrap(),
                    ),
                    _ => panic!("unexpected foundational operation"),
                }?;
                Ok(bmux_plugin_sdk::service_contract::encode_response(
                    &response,
                ))
            },
        ));
        let error = plugin.start_listener(&context).unwrap_err();
        assert_eq!(error.error.unwrap().code, "backend_unconfirmed");
        assert!(!plugin.listener_status().unwrap().enabled);
    }

    #[test]
    fn provider_routes_require_host_internal_not_local_or_remote() {
        use bmux_clients_plugin_api::clients_identity::StreamProvenance as Origin;
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        let pairing = Arc::new(
            pairing::PairingAuthority::open(
                &paths.state_dir,
                &paths.runtime_dir,
                iroh::SecretKey::generate().public().to_string(),
            )
            .unwrap(),
        );
        let admission = Arc::new(admission::AdmissionAuthority::new(pairing));
        let instance = admission.instance();
        let plugin = ConnectionsPlugin {
            admission: Some(admission),
            ..ConnectionsPlugin::default()
        };
        let mut context = service_context(
            &paths,
            "probe",
            bmux_plugin_sdk::encode_service_message(&()).unwrap(),
        );
        context.request.service.interface_id = "admission-provider-state".into();
        assert!(plugin.invoke_service(context.clone()).error.is_some());
        for origin in [
            Origin::Local,
            Origin::Invalid,
            Origin::Admitted {
                subject_id: uuid::Uuid::new_v4(),
                lease_id: uuid::Uuid::new_v4(),
            },
        ] {
            let _router = provenance_router(origin);
            assert!(plugin.invoke_service(context.clone()).error.is_some());
        }
        let _router = provenance_router(Origin::HostInternal);
        let response = plugin.invoke_service(context);
        assert!(response.error.is_none());
        assert_eq!(
            bmux_plugin_sdk::decode_service_message::<uuid::Uuid>(&response.payload).unwrap(),
            instance
        );
        let context = service_context(&paths, "create-invitation", Vec::new());
        assert!(pairing_owner(&context).is_err());
    }

    #[test]
    fn generated_pairing_routes_require_owner_and_read_durable_grants() {
        use bmux_connections_plugin_api::pairing_types::{DeviceAccess, DeviceAccessCheck};
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        let pairing = Arc::new(
            pairing::PairingAuthority::open(
                &paths.state_dir,
                &paths.runtime_dir,
                iroh::SecretKey::generate().public().to_string(),
            )
            .unwrap(),
        );
        let admission = Arc::new(admission::AdmissionAuthority::new(Arc::clone(&pairing)));
        let plugin = ConnectionsPlugin {
            pairing: Some(Arc::clone(&pairing)),
            admission_handle: Some(StreamAdmissionAuthorityHandle::new(admission.clone())),
            admission: Some(admission),
            ..ConnectionsPlugin::default()
        };
        let request = pairing_requests::CreateInvitationRequest {
            device_key: iroh::SecretKey::generate().public().to_string(),
            ttl_seconds: 300,
            access: DeviceAccess::FullLocalRuntime,
        };
        let mut context = service_context(
            &paths,
            "create-invitation",
            bmux_plugin_sdk::encode_service_message(&request).unwrap(),
        );
        context.request.service.interface_id = "pairing-commands".into();
        // Missing provenance is not local. Supplying a client ID is not proof either.
        assert!(plugin.invoke_service(context.clone()).error.is_some());
        context.caller_client_id = Some(uuid::Uuid::new_v4());
        assert!(plugin.invoke_service(context).error.is_some());
        let _router = trusted_local_router();
        let mut trusted = service_context(
            &paths,
            "create-invitation",
            bmux_plugin_sdk::encode_service_message(&request).unwrap(),
        );
        trusted.request.service.interface_id = "pairing-commands".into();
        assert!(plugin.invoke_service(trusted).error.is_none());
        let key = iroh::SecretKey::generate();
        let owner = pairing::LocalOwner::from_trusted_local_owner();
        let invitation = pairing
            .create_invitation(
                &owner,
                &key.public().to_string(),
                300,
                DeviceAccess::FullLocalRuntime,
            )
            .unwrap();
        let request = pairing_requests::RedeemInvitationRequest {
            invitation_id: invitation.invitation_id,
            secret: invitation.secret,
            signature: key.sign(&invitation.possession_message).to_bytes().to_vec(),
        };
        let mut context = service_context(
            &paths,
            "redeem-invitation",
            bmux_plugin_sdk::encode_service_message(&request).unwrap(),
        );
        context.request.service.interface_id = "pairing-commands".into();
        let response = plugin.invoke_service(context);
        assert!(response.error.is_none());
        let device: bmux_connections_plugin_api::pairing_types::Device =
            bmux_plugin_sdk::decode_service_message(&response.payload).unwrap();
        let request =
            bmux_connections_plugin_api::pairing_state::client::CheckDeviceAccessRequest {
                subject_id: uuid::Uuid::parse_str(&device.device_id).unwrap(),
            };
        let mut context = service_context(
            &paths,
            "check-device-access",
            bmux_plugin_sdk::encode_service_message(&request).unwrap(),
        );
        context.request.service.interface_id = "pairing-state".into();
        let response = plugin.invoke_service(context.clone());
        assert!(response.error.is_none());
        let check: DeviceAccessCheck =
            bmux_plugin_sdk::decode_service_message(&response.payload).unwrap();
        assert_eq!(check.access, DeviceAccess::FullLocalRuntime);
        pairing.revoke(&owner, &device.device_id).unwrap();
        let response = plugin.invoke_service(context);
        let check: DeviceAccessCheck =
            bmux_plugin_sdk::decode_service_message(&response.payload).unwrap();
        assert_eq!(check.access, DeviceAccess::Denied);
        assert!(plugin.admission_handle().is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn plugin_service_routes_resolution_and_invocation() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(root.path());
        std::fs::create_dir_all(&paths.config_dir).expect("config dir");
        std::fs::write(paths.config_file(), "").expect("config file");
        let server = std::sync::Arc::new(BmuxServer::from_config_paths(&paths));
        server
            .register_service_handler(
                "example.echo",
                bmux_ipc::InvokeServiceKind::Query,
                "echo/v1",
                "echo",
                |_route, _context, payload| async move { Ok(payload) },
            )
            .expect("register service");
        let running = std::sync::Arc::clone(&server);
        let task = tokio::spawn(async move { running.run().await });
        // Bind creates the path before listen/handshake readiness. Probe the
        // connection, not the filesystem, without replaying the tested service.
        let probe = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match BmuxClient::connect_with_paths(&paths, "connections-test-ready").await {
                    Ok(client) => break client,
                    Err(error) => {
                        assert!(
                            !task.is_finished(),
                            "server stopped before readiness: {error}"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }
            }
        })
        .await
        .expect("server readiness deadline");
        drop(probe);

        let resolve = bmux_connections_plugin_api::connections_state::client::ResolveRequest {
            target: "local".to_string(),
        };
        let resolve_payload =
            bmux_plugin_sdk::encode_service_message(&resolve).expect("encode resolve");
        let response = ConnectionsPlugin::default().invoke_service(service_context(
            &paths,
            "resolve",
            resolve_payload,
        ));
        assert!(response.error.is_none());
        let resolved: Result<
            bmux_connections_plugin_api::connection_types::ResolvedEndpoint,
            bmux_connections_plugin_api::connection_types::ConnectionError,
        > = bmux_plugin_sdk::decode_service_message(&response.payload).expect("decode resolve");
        assert_eq!(resolved.expect("resolved").address, "local");

        let request = InvokeServiceRequest {
            invocation: bmux_connections_plugin_api::connection_types::ServiceInvocation {
                target: "local".to_string(),
                capability: "example.echo".to_string(),
                kind: ServiceInvokeKind::InvokeQuery,
                interface_id: "echo/v1".to_string(),
                operation: "echo".to_string(),
                payload: b"routed payload".to_vec(),
                options: None,
            },
        };
        let payload = bmux_plugin_sdk::encode_service_message(&request).expect("encode invoke");
        let response = ConnectionsPlugin::default().invoke_service(service_context(
            &paths,
            "invoke-service",
            payload,
        ));
        assert!(response.error.is_none());
        let invoked: Result<Vec<u8>, ConnectionError> =
            bmux_plugin_sdk::decode_service_message(&response.payload).expect("decode invoke");
        assert_eq!(invoked.expect("invoked"), b"routed payload");

        server.request_shutdown();
        task.await.expect("server task join").expect("server run");
    }

    async fn cached_test_client(
        identity: &str,
        deadline: tokio::time::Instant,
    ) -> EndpointConnectionLease<BmuxClient> {
        connection_pool()
            .acquire(identity.to_owned(), deadline, || async {
                Err::<BmuxClient, ConnectionError>(ConnectionError::Cancelled {
                    target: "unexpected reconnect".into(),
                })
            })
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_endpoint_invokes_typed_service_end_to_end() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(root.path());
        std::fs::create_dir_all(&paths.config_dir).expect("config dir");
        std::fs::write(paths.config_file(), "").expect("config file");
        let server = std::sync::Arc::new(BmuxServer::from_config_paths(&paths));
        server
            .register_service_handler(
                "example.echo",
                bmux_ipc::InvokeServiceKind::Query,
                "echo/v1",
                "echo",
                |_route, _context, payload| async move {
                    if payload == b"reused payload" {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Ok(payload)
                },
            )
            .expect("register service");
        let running = std::sync::Arc::clone(&server);
        let task = tokio::spawn(async move { running.run().await });
        // A bound socket path can exist before listen() makes it connectable.
        // Wait for an actual connection rather than racing that startup interval.
        let ready = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if tokio::net::UnixStream::connect(paths.server_socket())
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(ready.is_ok(), "local server did not become connectable");

        let request = InvokeServiceRequest {
            invocation: bmux_connections_plugin_api::connection_types::ServiceInvocation {
                target: "local".to_string(),
                capability: "example.echo".to_string(),
                kind: ServiceInvokeKind::InvokeQuery,
                interface_id: "echo/v1".to_string(),
                operation: "echo".to_string(),
                payload: b"typed payload".to_vec(),
                options: None,
            },
        };
        let response = invoke_service(
            &BmuxConfig::default(),
            &paths,
            &CancellationToken::new(),
            request,
        )
        .expect("local invocation");
        assert_eq!(response, b"typed payload");

        let identity = target::resolve_internal(&BmuxConfig::default(), "local")
            .expect("local target")
            .connection_identity(&BmuxConfig::default(), &paths);
        assert_eq!(connection_pool().endpoint_counts(&identity), (0, 1, 1));

        // Seed a pooled client with a request timeout shorter than the next
        // operation. Its setup/default budget must not cap that invocation.
        let pool_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut pooled = cached_test_client(&identity, pool_deadline).await;
        pooled.set_request_timeout(std::time::Duration::from_millis(1));
        drop(pooled);

        let second = InvokeServiceRequest {
            invocation: bmux_connections_plugin_api::connection_types::ServiceInvocation {
                target: "local".to_string(),
                capability: "example.echo".to_string(),
                kind: ServiceInvokeKind::InvokeQuery,
                interface_id: "echo/v1".to_string(),
                operation: "echo".to_string(),
                payload: b"reused payload".to_vec(),
                options: Some(InvocationOptions {
                    timeout_ms: 1_000,
                    max_attempts: 1,
                    retry_backoff_ms: 0,
                }),
            },
        };
        let response = invoke_service(
            &BmuxConfig::default(),
            &paths,
            &CancellationToken::new(),
            second,
        )
        .expect("reused local invocation");
        assert_eq!(response, b"reused payload");
        assert_eq!(connection_pool().endpoint_counts(&identity), (0, 1, 1));
        let mut pooled = cached_test_client(&identity, pool_deadline).await;
        assert_eq!(
            pooled.set_request_timeout(std::time::Duration::from_secs(5)),
            std::time::Duration::from_millis(1)
        );
        drop(pooled);

        server.request_shutdown();
        task.await.expect("server task join").expect("server run");
    }

    #[test]
    fn invocation_options_are_bounded_and_default_to_one_attempt() {
        assert_eq!(
            normalized_invocation_options(None),
            InvocationOptions {
                timeout_ms: DEFAULT_INVOCATION_TIMEOUT_MS,
                max_attempts: 1,
                retry_backoff_ms: 0,
            }
        );
        assert_eq!(
            normalized_invocation_options(Some(&InvocationOptions {
                timeout_ms: 17,
                max_attempts: u32::MAX,
                retry_backoff_ms: u64::MAX,
            })),
            InvocationOptions {
                timeout_ms: 17,
                max_attempts: MAX_INVOCATION_ATTEMPTS,
                retry_backoff_ms: MAX_RETRY_BACKOFF_MS,
            }
        );
    }

    #[test]
    fn cancelled_invocation_fails_before_endpoint_resolution() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(root.path());
        let request = InvokeServiceRequest {
            invocation: bmux_connections_plugin_api::connection_types::ServiceInvocation {
                target: "missing".to_string(),
                capability: "example.echo".to_string(),
                kind: ServiceInvokeKind::InvokeQuery,
                interface_id: "echo/v1".to_string(),
                operation: "echo".to_string(),
                payload: Vec::new(),
                options: None,
            },
        };
        assert_eq!(
            invoke_service(
                &BmuxConfig::default(),
                &paths,
                &CancellationToken::cancelled(),
                request,
            ),
            Err(ConnectionError::Cancelled {
                target: "missing".to_string(),
            })
        );
    }

    #[test]
    fn zero_deadline_fails_before_endpoint_resolution() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(root.path());
        let request = InvokeServiceRequest {
            invocation: bmux_connections_plugin_api::connection_types::ServiceInvocation {
                target: "missing".to_string(),
                capability: "example.echo".to_string(),
                kind: ServiceInvokeKind::InvokeQuery,
                interface_id: "echo/v1".to_string(),
                operation: "echo".to_string(),
                payload: Vec::new(),
                options: Some(InvocationOptions {
                    timeout_ms: 0,
                    max_attempts: 1,
                    retry_backoff_ms: 0,
                }),
            },
        };
        assert_eq!(
            invoke_service(
                &BmuxConfig::default(),
                &paths,
                &CancellationToken::new(),
                request,
            ),
            Err(ConnectionError::TimedOut {
                target: "missing".to_string(),
                phase: "request".to_string(),
                timeout_ms: 0,
            })
        );
    }

    #[test]
    fn load_config_uses_host_candidate_chain() {
        let root = tempfile::tempdir().expect("tempdir");
        let canonical = root.path().join("canonical");
        let fallback = root.path().join("fallback");
        std::fs::create_dir_all(&fallback).expect("fallback dir");
        std::fs::write(
            fallback.join("bmux.toml"),
            "[connections]\ndefault_target = 'remote'\n",
        )
        .expect("fallback config");
        let connection = HostConnectionInfo {
            config_dir: canonical.to_string_lossy().into_owned(),
            config_dir_candidates: vec![
                canonical.to_string_lossy().into_owned(),
                fallback.to_string_lossy().into_owned(),
            ],
            runtime_dir: root.path().join("runtime").to_string_lossy().into_owned(),
            data_dir: root.path().join("data").to_string_lossy().into_owned(),
            state_dir: root.path().join("state").to_string_lossy().into_owned(),
        };
        let config = target::load_config(&connection).expect("fallback config");
        assert_eq!(config.connections.default_target.as_deref(), Some("remote"));
    }

    #[test]
    fn context_paths_preserve_all_plugin_host_roots() {
        let connection = bmux_plugin_sdk::HostConnectionInfo {
            config_dir: "/config".to_string(),
            config_dir_candidates: vec![PathBuf::from("/config")]
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            runtime_dir: "/runtime".to_string(),
            data_dir: "/data".to_string(),
            state_dir: "/state".to_string(),
        };
        let paths = target::context_paths(&connection);
        assert_eq!(paths.config_dir, PathBuf::from("/config"));
        assert_eq!(paths.runtime_dir, PathBuf::from("/runtime"));
        assert_eq!(paths.data_dir, PathBuf::from("/data"));
        assert_eq!(paths.state_dir, PathBuf::from("/state"));
    }
}

bmux_plugin_sdk::export_plugin!(ConnectionsPlugin, include_str!("../plugin.toml"));
