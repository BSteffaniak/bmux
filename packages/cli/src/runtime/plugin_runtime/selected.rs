//! Application routing over the shared scoped executor. Native v1 is not a
//! fallback: only explicitly process-packaged plugins use the legacy adapter.
#[cfg(test)]
#[path = "selected_tests.rs"]
mod selected_tests;
use super::load_process_plugin;
use bmux_config::BmuxConfig;
use bmux_plugin::PluginRuntime;
use bmux_plugin_runtime::PluginRuntimeScopeId;
use bmux_plugin_sdk::abi::{NativeOperation, NativeOutcome};
use bmux_plugin_sdk::{
    CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION, HostMetadata, HostScope,
    NativeCommandContext, NativeLifecycleContext, PluginCommandOutcome, PluginEvent,
};
use bmux_plugin_sdk::{PluginError, TypedServiceRegistrationContext, TypedServiceRegistry};
use std::{collections::BTreeMap, future::Future, sync::Arc};

pub(in crate::runtime) struct ExecutionOwner<'a> {
    pub cli_command: Option<&'a str>,
    pub runtime_owner: Option<crate::runtime_owner::RuntimeOwner>,
    pub runtime: PluginRuntimeScopeId,
    pub host_state: Arc<bmux_plugin::PluginStateRegistry>,
    pub loader: &'a bmux_plugin::NativePluginLoader,
    pub definitions: &'a BTreeMap<String, bmux_plugin_sdk::definition::PluginDefinition>,
    pub approval_store: Option<&'a bmux_config::artifact_approval::ArtifactApprovalStore>,
    pub runtime_dir: &'a std::path::Path,
}

#[derive(Clone)]
pub(in crate::runtime) struct ProductionPlugin {
    pub registered: bmux_plugin::RegisteredPlugin,
    pub declaration: bmux_plugin::PluginDeclaration,
    backend: Backend,
    active: Arc<std::sync::atomic::AtomicBool>,
    kernel_grant: Option<super::kernel_policy::InstanceKernelGrant>,
    lifecycle_authority:
        Arc<std::sync::OnceLock<crate::runtime::plugin_kernel::AuthenticatedServiceInvocation>>,
    builtins: Arc<std::sync::OnceLock<Arc<bmux_plugin::InvocationHostBuiltins>>>,
}
#[derive(Clone)]
enum Backend {
    Scoped(Arc<bmux_plugin::LoadedExecution>),
    Process(Arc<bmux_plugin::LoadedPlugin>),
}
fn failure(details: impl Into<String>) -> PluginError {
    PluginError::ServiceProtocol {
        details: details.into(),
    }
}

fn observation_ack_error(id: u64, outcome: &NativeOutcome, error: &PluginError) -> PluginError {
    match outcome {
        NativeOutcome::Error { code, message } => failure(format!(
            "plugin operation {id} failed ({code}): {message}; retained cleanup/acknowledgement blocker: {error}"
        )),
        _ => failure(format!(
            "plugin operation {id} outcome acknowledgement failed; retained, not retried: {error}"
        )),
    }
}

/// Same-instance re-entry policy for one provider invocation.
///
/// Only a query whose operation the provider's declaration marks `@reentrant`
/// (BPDL-generated evidence) may enter while an ancestor entry of the same
/// instance is live; everything else keeps the default rejection.
fn provider_reentry(
    declaration: &bmux_plugin::PluginDeclaration,
    request: &bmux_plugin_sdk::ServiceRequest,
) -> bmux_plugin::CallbackReentry {
    let service = &request.service;
    let declared = service.kind == bmux_plugin_sdk::ServiceKind::Query
        && declaration.services.iter().any(|declared| {
            declared.capability == service.capability
                && declared.kind == service.kind
                && declared.interface_id == service.interface_id
                && declared.is_reentrant_query(&request.operation)
        });
    if declared {
        bmux_plugin::CallbackReentry::AllowQuery
    } else {
        bmux_plugin::CallbackReentry::Reject
    }
}

#[derive(Clone)]
struct KernelServices {
    declaration: bmux_plugin::PluginDeclaration,
    instance: bmux_plugin_runtime::PluginInstanceId,
    authority: Option<crate::runtime::plugin_kernel::AuthenticatedServiceInvocation>,
    services: Vec<bmux_plugin_sdk::RegisteredService>,
    builtins: Arc<std::sync::OnceLock<Arc<bmux_plugin::InvocationHostBuiltins>>>,
    command_route: Option<crate::runtime::plugin_kernel::CommandDispatch>,
    /// The instance's selected host reactor, never the polling thread's.
    runtime: tokio::runtime::Handle,
}

/// The host reactor explicitly selected for a scoped instance's callbacks.
fn host_runtime(
    execution: &bmux_plugin::LoadedExecution,
) -> bmux_plugin_sdk::Result<tokio::runtime::Handle> {
    execution
        .callback_runtime()
        .ok_or_else(|| failure("selected callback runtime is not bound"))
}

impl KernelServices {
    fn declares_capability(&self, requested: &str) -> bool {
        self.declaration
            .required_capabilities
            .iter()
            .chain(&self.declaration.optional_capabilities)
            .chain(&self.declaration.provided_capabilities)
            .any(|capability| capability.as_str() == requested)
    }

    /// Host dispatch for one command operation bound to `binding`.
    ///
    /// An async command body (and blocking work it awaits) runs outside the
    /// author's callback entry. Such calls take this command's own selected
    /// dispatch: the same declaration, capability checks, authority and
    /// issuing route, only without entry ancestry (so nested plugin commands
    /// still require a live entry). Calls made inside the entry keep the
    /// entry route.
    fn command_dispatch(
        self,
        binding: &bmux_plugin::CallbackBinding,
    ) -> Arc<dyn bmux_plugin_sdk::HostServiceDispatch> {
        let instance = Arc::new(self.clone());
        binding.entry_or_instance_dispatch(
            Arc::new(move |entry| Ok(Arc::new(self.with_callback_parent(entry)))),
            instance,
        )
    }
    fn with_callback_parent(&self, entry: &bmux_plugin::CallbackEntryLease) -> Self {
        Self {
            declaration: self.declaration.clone(),
            instance: self.instance,
            authority: self
                .authority
                .clone()
                .map(|authority| authority.with_callback_parent(entry)),
            services: self.services.clone(),
            builtins: self.builtins.clone(),
            command_route: self
                .command_route
                .clone()
                .map(|route| route.with_parent(entry)),
            runtime: self.runtime.clone(),
        }
    }
    /// Answer host-intrinsic facts about this exact admitted invocation. No
    /// declaration or kernel grant is involved: the answer comes only from the
    /// invocation's authenticated server context or its issuing command route.
    fn invocation_intrinsic(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
        request: &bmux_plugin_sdk::HostServiceRequest<'_>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        if request.kind != bmux_plugin_sdk::ServiceKind::Query
            || request.interface != bmux_plugin_sdk::CORE_INVOCATION_INTERFACE_V1
            || request.operation != bmux_plugin_sdk::CORE_INVOCATION_STREAM_ORIGIN_OPERATION_V1
            || !request.payload.is_empty()
        {
            return Err(failure("unsupported host invocation intrinsic"));
        }
        let origin = if let Some(authority) = &self.authority {
            authority.stream_origin(invocation)?
        } else if let Some(route) = &self.command_route {
            route.stream_origin(invocation)?
        } else {
            return Err(failure(
                "invocation origin requires an authenticated selected context",
            ));
        };
        bmux_plugin_sdk::encode_service_message(&origin)
    }

    async fn invoke_owned(
        self,
        invocation: bmux_plugin_runtime::InvocationScope,
        request: bmux_plugin_sdk::HostServiceOwnedRequest,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        request.validate()?;
        if invocation.instance_id() != self.instance {
            return Err(failure("host service caller instance mismatch"));
        }
        let admission = invocation
            .try_admit()
            .map_err(|error| failure(error.to_string()))?;
        // Keep scope ownership and authoritative server context through the
        // blocking callback. No request metadata creates authority, no result
        // failure triggers replay, and drop never claims to undo an effect.
        let runtime = self.runtime.clone();
        crate::runtime::plugin_kernel::AuthenticatedServiceInvocation::dispatch_host_callback(
            runtime,
            move || {
                let _admission = admission;
                bmux_plugin_sdk::HostServiceDispatch::invoke(
                    &self,
                    &invocation,
                    request.as_request(),
                )
            },
        )
        .await
    }

    /// Run a plugin command requested from inside another plugin's callback.
    fn invoke_nested_cli_command(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
        request: bmux_plugin_sdk::HostServiceRequest<'_>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        // Nested commands must inherit this actual entry's issuing route,
        // never the instance-cached builtin executor or another TLS route.
        // A server service callback has an authenticated server invocation,
        // not a CLI transport. Dispatch on that invocation only while its
        // actual callback entry is live; never fall back to lifecycle roots.
        if self.command_route.is_none()
            && let Some(authority) = &self.authority
        {
            let _parent = authority.callback_parent()?.ok_or_else(|| {
                failure("nested plugin command requires live callback dependency")
            })?;
            let bytes = bmux_ipc::encode(&bmux_ipc::Request::InvokeService {
                capability: request.capability.into(),
                kind: bmux_ipc::InvokeServiceKind::Command,
                interface_id: request.interface.into(),
                operation: request.operation.into(),
                payload: request.payload.to_vec(),
            })
            .map_err(|error| failure(error.to_string()))?;
            return match authority
                .clone()
                .with_plugin_caller(&self.declaration.id)
                .execute_response_scoped(invocation, &bytes)?
            {
                bmux_ipc::ResponsePayload::ServiceInvoked { payload } => Ok(payload),
                _ => Err(failure("unexpected authenticated nested command response")),
            };
        }
        let route = match &self.command_route {
            Some(route) if route.parent().is_some() => route.clone(),
            Some(_) => {
                return Err(failure(
                    "nested plugin command requires live callback dependency",
                ));
            }
            None => {
                return Err(failure(
                    "nested plugin command requires selected invocation transport",
                ));
            }
        };
        crate::runtime::plugin_kernel::scoped_cli_executor_with_dispatch(Some(route))
            .map_err(|error| failure(error.to_string()))?
            .invoke(request)
    }
}

impl bmux_plugin_sdk::HostServiceDispatch for KernelServices {
    fn selected_service_availability(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
        key: bmux_plugin_sdk::typed_dispatch::ServiceAvailabilityKey,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        'static,
        bmux_plugin_sdk::Result<bmux_plugin_sdk::typed_dispatch::SelectedServiceAvailability>,
    > {
        let host = self.clone();
        let invocation = invocation.clone();
        Box::pin(async move {
            key.validate()?;
            if invocation.instance_id() != host.instance {
                return Err(failure("host availability caller instance mismatch"));
            }
            let _admission = invocation
                .try_admit()
                .map_err(|error| failure(error.to_string()))?;
            if !host.declares_capability(&key.capability) {
                return Err(failure(
                    "host availability capability not declared by caller",
                ));
            }
            let authority = host
                .authority
                .as_ref()
                .ok_or_else(|| failure("availability requires authenticated selected context"))?;
            authority.selected_service_availability(&invocation, &key)
        })
    }

    fn invoke_async(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
        request: bmux_plugin_sdk::HostServiceOwnedRequest,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'static, bmux_plugin_sdk::Result<Vec<u8>>> {
        Box::pin(self.clone().invoke_owned(invocation.clone(), request))
    }

    fn invoke(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
        request: bmux_plugin_sdk::HostServiceRequest<'_>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        if invocation.instance_id() != self.instance {
            return Err(failure("host service caller instance mismatch"));
        }
        let _admission = invocation
            .try_admit()
            .map_err(|error| failure(error.to_string()))?;
        if request.capability == bmux_plugin_sdk::CORE_INVOCATION_CAPABILITY {
            return self.invocation_intrinsic(invocation, &request);
        }
        if !self.declares_capability(request.capability) {
            return Err(failure("host service capability not declared by caller"));
        }
        if request.capability == bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY
            && request.kind == bmux_plugin_sdk::ServiceKind::Command
            && request.interface == bmux_plugin_sdk::CORE_CLI_COMMAND_INTERFACE_V1
            && request.operation == bmux_plugin_sdk::CORE_CLI_COMMAND_RUN_PLUGIN_OPERATION_V1
        {
            return self.invoke_nested_cli_command(invocation, request);
        }
        let selected_service = self.services.iter().find(|service| {
            service.capability.as_str() == request.capability
                && service.kind == request.kind
                && service.interface_id == request.interface
        });
        if let Some(builtins) = self.builtins.get() {
            if let Some(response) = builtins.try_invoke(invocation, &request)? {
                return Ok(response);
            }
        } else if let Some(service) = selected_service
            && matches!(service.provider, bmux_plugin_sdk::ProviderId::Host)
        {
            return Err(failure(
                "selected host primitives have not been bound to runtime roots",
            ));
        }
        let kind = match request.kind {
            bmux_plugin_sdk::ServiceKind::Query => bmux_ipc::InvokeServiceKind::Query,
            bmux_plugin_sdk::ServiceKind::Command => bmux_ipc::InvokeServiceKind::Command,
            bmux_plugin_sdk::ServiceKind::Event => {
                return Err(failure("unsupported host service kind"));
            }
        };
        let bytes = bmux_ipc::encode(&bmux_ipc::Request::InvokeService {
            capability: request.capability.into(),
            kind,
            interface_id: request.interface.into(),
            operation: request.operation.into(),
            payload: request.payload.to_vec(),
        })
        .map_err(|error| failure(error.to_string()))?;
        if let Some(authority) = &self.authority {
            return match authority
                .clone()
                .with_plugin_caller(&self.declaration.id)
                .execute_response_scoped(invocation, &bytes)?
            {
                bmux_ipc::ResponsePayload::ServiceInvoked { payload } => Ok(payload),
                _ => Err(failure("unexpected authenticated service response")),
            };
        }
        if let Some(route) = &self.command_route {
            // Attribution is the declaration this host bound to the admitted
            // instance (checked above), never anything in the request.
            return route.invoke(invocation, Some(&self.declaration.id), &request);
        }
        Err(failure(
            "scoped host service invocation has no explicit authenticated dispatch context",
        ))
    }
}

// This routing facade preserves the SDK's public PluginError contract; boxing
// it here would change the loader/service interface and discard typed errors.
#[allow(clippy::result_large_err)]
impl ProductionPlugin {
    pub(in crate::runtime) fn load(
        plugin: &bmux_plugin::RegisteredPlugin,
        host: &HostMetadata,
        capabilities: &BTreeMap<HostScope, bmux_plugin::CapabilityProvider>,
        config: &BmuxConfig,
        owner: ExecutionOwner<'_>,
    ) -> bmux_plugin_sdk::Result<Self> {
        Self::load_component(
            plugin,
            host,
            capabilities,
            config,
            owner,
            super::deployed_component_role(&plugin.manifest).map_err(|error| *error)?,
        )
    }

    pub(in crate::runtime) fn load_component(
        plugin: &bmux_plugin::RegisteredPlugin,
        host: &HostMetadata,
        capabilities: &BTreeMap<HostScope, bmux_plugin::CapabilityProvider>,
        config: &BmuxConfig,
        owner: ExecutionOwner<'_>,
        role: bmux_plugin_sdk::definition::ComponentRole,
    ) -> bmux_plugin_sdk::Result<Self> {
        let mut kernel_grant = None;
        let builtins = Arc::new(std::sync::OnceLock::new());
        let backend = if plugin.manifest.runtime == PluginRuntime::Process {
            Backend::Process(Arc::new(load_process_plugin(plugin, host, capabilities)?))
        } else {
            let runtime_owner = owner.runtime_owner.ok_or_else(|| failure(
                "selected plugin execution requires actual primary runtime owner; use run_cli_with_runtime"))?;
            let mut inventory = bmux_plugin::PluginCandidateInventory::new();
            inventory.add_registered_plugin(plugin)?;
            let selected = inventory.select(&bmux_plugin::PluginSelectionRequest {
                plugin_id: plugin.manifest.id.clone(),
                role,
                preference: if plugin.bundled_static {
                    bmux_plugin::PluginCandidatePreference::Bundled
                } else {
                    bmux_plugin::PluginCandidatePreference::Dynamic {
                        manifest_path: plugin.manifest_path.clone(),
                    }
                },
                plugin_api: CURRENT_PLUGIN_API_VERSION,
                native_abi: CURRENT_PLUGIN_ABI_VERSION,
                contracts: Vec::new(),
            })?;
            let kernel_approval = super::trusted_bundled_kernel_policy(plugin)?
                .map(|policy| policy.approve(&selected))
                .transpose()?;
            let native_approval = owner
                .approval_store
                .map(|store| {
                    super::kernel_policy::NativeKernelApproval::for_selected(
                        store,
                        &selected,
                        owner.runtime_dir,
                        matches!(
                            config.plugins.sources.get(&plugin.manifest.id),
                            Some(bmux_config::PluginSourceConfig::Dynamic { .. })
                        ),
                    )
                })
                .transpose()?
                .flatten();
            let capabilities = capabilities
                .keys()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            // Startup explicitly selects trusted native execution; shared loading
            // pins copied artifact bytes and verifies embedded identity.
            let execution = unsafe {
                if let Some(definition) = owner.definitions.get(&plugin.manifest.id) {
                    owner.loader.load_selected_execution_with_definition(
                        &selected,
                        owner.runtime,
                        &capabilities,
                        definition.clone(),
                        tokio::runtime::Handle::current(),
                        owner.cli_command,
                    )
                } else {
                    owner.loader.load_selected_execution_with_owner(
                        &selected,
                        owner.runtime,
                        &capabilities,
                        tokio::runtime::Handle::current(),
                        owner.cli_command,
                    )
                }
            }?;
            execution
                .bind_callback_runtime_owner(runtime_owner.handle().clone(), runtime_owner.pin())?;
            kernel_grant = if let Some(approval) = native_approval {
                Some(approval.bind_created(&execution, owner.runtime)?)
            } else {
                kernel_approval
                    .map(|approval| approval.bind_created(&execution))
                    .transpose()?
            };
            execution.bind_host_state(owner.host_state)?;
            Backend::Scoped(execution)
        };
        Ok(Self {
            registered: plugin.clone(),
            declaration: plugin.declaration.clone(),
            backend,
            active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            kernel_grant,
            lifecycle_authority: Arc::new(std::sync::OnceLock::new()),
            builtins,
        })
    }

    async fn observe(
        execution: &Arc<bmux_plugin::LoadedExecution>,
        id: u64,
    ) -> bmux_plugin_sdk::Result<NativeOutcome> {
        Self::observe_with_cleanup(execution, id, false).await
    }

    async fn observe_with_cleanup(
        execution: &Arc<bmux_plugin::LoadedExecution>,
        id: u64,
        deactivation: bool,
    ) -> bmux_plugin_sdk::Result<NativeOutcome> {
        // Waker-driven: the operation (or its loader-side settlement,
        // cancellation, deadline, or legacy-guest repoll) wakes this signal.
        // No observer-imposed deadline: timeouts are explicit caller policy.
        let signal = bmux_plugin::OperationSignal::new();
        let outcome = loop {
            let waker = signal.waker();
            let owned = Arc::clone(execution);
            // Legacy author hooks and services may synchronously call typed
            // hosts while being polled. Poll them on the bounded callback
            // executor, never a Tokio worker; keep the actual observer waker.
            let polled = crate::runtime::plugin_kernel::AuthenticatedServiceInvocation::dispatch_host_callback(
                host_runtime(execution)?,
                move || owned.poll(id, &mut std::task::Context::from_waker(&waker)),
            ).await?;
            if let std::task::Poll::Ready(outcome) = polled {
                break outcome;
            }
            signal.wait().await?;
        };
        let acknowledgement = if deactivation
            && matches!(outcome, NativeOutcome::Unit)
            && execution
                .scope()
                .state()
                .map_err(|error| failure(error.to_string()))?
                == bmux_plugin_runtime::LifecycleState::CleanupRequired
        {
            // Only successful terminal deactivation can prove repair. The
            // loader checks retirement/resources before discarding failures.
            execution.acknowledge_repaired_cleanup(id).await
        } else {
            execution.acknowledge(id).await
        };
        acknowledgement.map_err(|error| observation_ack_error(id, &outcome, &error))?;
        match outcome {
            NativeOutcome::Error { code, message } => {
                Err(failure(format!("plugin outcome {code}: {message}")))
            }
            outcome => Ok(outcome),
        }
    }

    fn observe_sync(
        execution: &bmux_plugin::LoadedExecution,
        id: u64,
    ) -> bmux_plugin_sdk::Result<NativeOutcome> {
        // Synchronous CLI callbacks cannot enter a second async runtime. Block
        // on a waker-driven signal: no periodic polling and no hidden deadline.
        let signal = bmux_plugin::OperationSignal::new();
        let waker = signal.waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let outcome = loop {
            if let std::task::Poll::Ready(outcome) = execution.poll(id, &mut cx)? {
                break outcome;
            }
            signal.wait_blocking()?;
        };
        let mut ack = Box::pin(execution.acknowledge(id));
        loop {
            if let std::task::Poll::Ready(result) = ack.as_mut().poll(&mut cx) {
                result.map_err(|error| observation_ack_error(id, &outcome, &error))?;
                break;
            }
            signal.wait_blocking()?;
        }
        match outcome {
            NativeOutcome::Error { code, message } => {
                Err(failure(format!("plugin outcome {code}: {message}")))
            }
            outcome => Ok(outcome),
        }
    }

    fn bind_host_builtins(
        &self,
        connection: &bmux_plugin_sdk::HostConnectionInfo,
        services: &[bmux_plugin_sdk::RegisteredService],
        settings: &BTreeMap<String, toml::Value>,
    ) -> bmux_plugin_sdk::Result<()> {
        if self.builtins.get().is_some() {
            return Ok(());
        }
        let granted: Vec<HostScope> = self
            .declaration
            .required_capabilities
            .iter()
            .chain(self.declaration.optional_capabilities.iter())
            .chain(self.declaration.provided_capabilities.iter())
            .cloned()
            .collect();
        let cli = if granted.iter().any(|capability: &HostScope| {
            capability.as_str() == bmux_plugin_sdk::CORE_CLI_COMMAND_CAPABILITY
        }) {
            Some(
                crate::runtime::plugin_kernel::scoped_cli_executor().map_err(|error| {
                    failure(format!("selected CLI executor binding failed: {error:#}"))
                })?,
            )
        } else {
            None
        };
        let primitives = bmux_plugin::InvocationHostBuiltins::new(
            bmux_plugin::PluginId::new(self.declaration.id.as_str())?,
            connection.clone(),
            services.to_vec(),
            granted,
            settings.clone(),
            cli,
        )?;
        self.builtins
            .set(Arc::new(primitives))
            .map_err(|_| failure("host primitive binding raced; instance requires recovery"))?;
        Ok(())
    }

    pub(in crate::runtime) fn bind_lifecycle_authority(
        &self,
        context: bmux_server::ServiceInvokeContext,
    ) -> bmux_plugin_sdk::Result<()> {
        self.lifecycle_authority
            .set(crate::runtime::plugin_kernel::AuthenticatedServiceInvocation::new(context))
            .map_err(|_| failure("lifecycle authority already bound"))
    }

    /// Offer this instance's activations views of the host event bus and
    /// plugin state registry, each scoped to the activation. Dynamic
    /// artifacts receive only the event bus. A process plugin's declared
    /// publications reach the event bus; it returns `false` because the state
    /// registry stays in process.
    pub(in crate::runtime) fn bind_host_handles(
        &self,
        bus: &Arc<bmux_plugin::EventBus>,
        states: &Arc<bmux_plugin::PluginStateRegistry>,
    ) -> bmux_plugin_sdk::Result<bool> {
        match &self.backend {
            // In-process and dynamic instances both receive an
            // activation-scoped event bus (dynamic ones over the serialized
            // plug-rt port); the state registry carries in-process values
            // only, so dynamic artifacts activate without it.
            Backend::Scoped(execution) => {
                execution.bind_event_bus(bus)?;
                if execution.is_in_process() {
                    execution.bind_activation_binder(
                        bmux_plugin::state_registry_activation_binder(Arc::clone(states)),
                    )?;
                }
                Ok(true)
            }
            // A process plugin publishes its declared event kinds through its
            // protocol (`ProcessInvocationResponse::Publish`) onto this bus;
            // the in-process state registry cannot reach it.
            Backend::Process(plugin) => {
                plugin.bind_event_bus(Arc::clone(bus));
                Ok(false)
            }
        }
    }

    /// Register a trusted host activation binder before this instance activates.
    pub(in crate::runtime) fn bind_activation_binder(
        &self,
        binder: Arc<bmux_plugin::ActivationBinder>,
    ) -> bmux_plugin_sdk::Result<()> {
        match &self.backend {
            Backend::Scoped(execution) => execution.bind_activation_binder(binder),
            Backend::Process(_) => Err(failure(
                "host activation capabilities cannot reach a process-packaged plugin",
            )),
        }
    }

    /// Activate a host-owned instance once for its owner's lifetime. No
    /// per-command transport is captured as the activation's host route: each
    /// later command binds its own dispatch.
    pub(in crate::runtime) fn activate_owned(
        &self,
        context: &NativeCommandContext,
    ) -> bmux_plugin_sdk::Result<()> {
        let Backend::Scoped(execution) = &self.backend else {
            return Err(failure("host-owned activation requires a scoped instance"));
        };
        if self.active.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }
        let mut context = context.clone();
        context.host_kernel_bridge = None;
        self.bind_host_builtins(
            &context.connection,
            &context.services,
            &context.plugin_settings_map,
        )?;
        self.activate_scoped_command(execution, &context, None)
    }

    pub(in crate::runtime) async fn activate_async(
        &self,
        context: &NativeLifecycleContext,
    ) -> bmux_plugin_sdk::Result<i32> {
        self.bind_host_builtins(
            &context.connection,
            &context.services,
            &context.plugin_settings_map,
        )?;
        match &self.backend {
            Backend::Process(plugin) => plugin.activate(context),
            Backend::Scoped(execution) => {
                let lifecycle_host = Arc::new(KernelServices {
                    declaration: self.declaration.clone(),
                    instance: execution.scope().id(),
                    authority: self.lifecycle_authority.get().cloned(),
                    services: context.services.clone(),
                    builtins: Arc::clone(&self.builtins),
                    command_route: None,
                    runtime: host_runtime(execution)?,
                });
                execution.bind_host_dispatch(lifecycle_host.clone())?;
                let lifecycle_execution = Arc::downgrade(execution);
                execution.bind_lifecycle_callback_factory(Arc::new(move |root| {
                    let callback = if let Some(authority) = &lifecycle_host.authority {
                        if let Some(parent) = authority.callback_parent()? {
                            Some(
                                lifecycle_execution
                                    .upgrade()
                                    .ok_or_else(|| {
                                        failure("lifecycle execution owner unavailable")
                                    })?
                                    .bind_child_callback(
                                        &parent,
                                        root.invocation().clone(),
                                        bmux_plugin_sdk::ServiceCancellation::new(),
                                    )?,
                            )
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    let binding = callback.as_ref().unwrap_or(root);
                    let selected = lifecycle_host.clone();
                    let host = binding.host_dispatch(Arc::new(move |entry| {
                        Ok(Arc::new((selected).with_callback_parent(entry)))
                    }));
                    Ok(bmux_plugin::LifecycleCallbackDispatch {
                        callback,
                        host: Some(host),
                        kernel: None,
                    })
                }))?;
                let mut context = context.clone();
                context.host_kernel_bridge = None;
                let id = execution.begin_activation(context, self.registered.manifest.concurrency)
                    .map_err(|error| failure(format!(
                        "selected lifecycle activation admission failed: plugin={} error={error}",
                        self.registered.manifest.id,
                    )))?;
                match Self::observe(execution, id).await? {
                    NativeOutcome::Unit => {
                        self.active
                            .store(true, std::sync::atomic::Ordering::Release);
                        Ok(0)
                    }
                    _ => Err(failure("unexpected activation outcome")),
                }
            }
        }
    }
    pub(in crate::runtime) fn collect_typed_services(
        &self,
        context: TypedServiceRegistrationContext<'_>,
    ) -> bmux_plugin_sdk::Result<TypedServiceRegistry> {
        match &self.backend {
            Backend::Process(plugin) => Ok(plugin.collect_typed_services(context)),
            Backend::Scoped(execution) => execution.collect_typed_services(context),
        }
    }
    pub(in crate::runtime) fn collect_activation_contributions(
        &self,
    ) -> bmux_plugin_sdk::Result<Vec<bmux_plugin_sdk::PluginContribution>> {
        match &self.backend {
            Backend::Process(plugin) => plugin.collect_activation_contributions(),
            Backend::Scoped(execution) => match Self::observe_sync(
                execution,
                execution.begin(NativeOperation::Registrations)?,
            )? {
                NativeOutcome::Registrations(registrations) => Ok(registrations.contributions),
                _ => Err(failure("unexpected registration outcome")),
            },
        }
    }
    pub(in crate::runtime) fn cancel_background_tasks(&self) -> bmux_plugin_sdk::Result<()> {
        match &self.backend {
            // Process plugins own no host activation tasks.
            Backend::Process(_) => Ok(()),
            Backend::Scoped(execution) => execution.cancel_background_tasks(),
        }
    }
    pub(in crate::runtime) async fn drain_background_tasks(&self) -> bmux_plugin_sdk::Result<()> {
        match &self.backend {
            Backend::Process(_) => Ok(()),
            Backend::Scoped(execution) => execution.drain_background_tasks().await,
        }
    }
    pub(in crate::runtime) async fn deactivate_async(
        &self,
        context: &NativeLifecycleContext,
    ) -> bmux_plugin_sdk::Result<i32> {
        match &self.backend {
            Backend::Process(plugin) => plugin.deactivate(context),
            Backend::Scoped(execution) => {
                let mut context = context.clone();
                context.host_kernel_bridge = None;
                let id = execution.begin_deactivation(context).await?;
                match Self::observe_with_cleanup(execution, id, true).await? {
                    NativeOutcome::Unit => {
                        execution.destroy().await?;
                        Ok(0)
                    }
                    _ => Err(failure("unexpected deactivation outcome")),
                }
            }
        }
    }
    pub(in crate::runtime) fn dispatch_event(
        &self,
        event: &PluginEvent,
    ) -> bmux_plugin_sdk::Result<Option<i32>> {
        if !self.declaration.event_subscriptions.is_empty()
            && !self
                .declaration
                .event_subscriptions
                .iter()
                .any(|subscription| subscription.matches(event))
        {
            return Ok(None);
        }
        match &self.backend {
            Backend::Process(plugin) => plugin.dispatch_event(event),
            Backend::Scoped(execution) => {
                let invocation = execution.begin_invocation_scope()?;
                let binding = execution
                    .bind_root_callback(invocation, bmux_plugin_sdk::ServiceCancellation::new())?;
                let id = execution.begin_with_callback_dispatch(
                    NativeOperation::Event(event.clone()),
                    binding,
                    None,
                    None,
                )?;
                match Self::observe_sync(execution, id)? {
                    NativeOutcome::Unit => Ok(Some(0)),
                    _ => Err(failure("unexpected event outcome")),
                }
            }
        }
    }
    #[cfg(test)]
    pub(super) fn run_command_with_context_and_outcome(
        &self,
        command: &str,
        args: &[String],
        context: Option<&NativeCommandContext>,
    ) -> bmux_plugin_sdk::Result<(i32, PluginCommandOutcome)> {
        self.run_command_with_dispatch(command, args, context, None, None)
    }

    /// Activate a command-only scoped instance before its first command.
    /// Command-only activation needs the same selected host dispatcher as
    /// async lifecycle activation. The loader derives each command's owned
    /// port from this binding.
    fn activate_scoped_command(
        &self,
        execution: &Arc<bmux_plugin::LoadedExecution>,
        context: &NativeCommandContext,
        command_route: Option<&crate::runtime::plugin_kernel::CommandDispatch>,
    ) -> bmux_plugin_sdk::Result<()> {
        let lifecycle_host = Arc::new(KernelServices {
            declaration: self.declaration.clone(),
            instance: execution.scope().id(),
            authority: self.lifecycle_authority.get().cloned(),
            services: context.services.clone(),
            builtins: Arc::clone(&self.builtins),
            command_route: command_route.cloned(),
            runtime: host_runtime(execution)?,
        });
        execution.bind_host_dispatch(lifecycle_host.clone())?;
        let lifecycle_execution = Arc::downgrade(execution);
        execution.bind_lifecycle_callback_factory(Arc::new(move |root| {
            let callback = if let Some(parent) = lifecycle_host
                .command_route
                .as_ref()
                .and_then(crate::runtime::plugin_kernel::CommandDispatch::parent)
            {
                Some(
                    lifecycle_execution
                        .upgrade()
                        .ok_or_else(|| failure("nested lifecycle execution unavailable"))?
                        .bind_child_callback(
                            &parent,
                            root.invocation().clone(),
                            bmux_plugin_sdk::ServiceCancellation::new(),
                        )?,
                )
            } else {
                None
            };
            let binding = callback.as_ref().unwrap_or(root);
            let selected = lifecycle_host.clone();
            let host = binding.host_dispatch(Arc::new(move |entry| {
                Ok(Arc::new((selected).with_callback_parent(entry)))
            }));
            Ok(bmux_plugin::LifecycleCallbackDispatch {
                callback,
                host: Some(host),
                kernel: None,
            })
        }))?;
        let activation =
            execution.begin_command_activation(context, self.registered.manifest.concurrency)?;
        if !matches!(
            Self::observe_sync(execution, activation)?,
            NativeOutcome::Unit
        ) {
            return Err(failure("unexpected command activation outcome"));
        }
        // observe_sync acknowledges the lifecycle operation before
        // command admission on this same scoped implementation.
        self.active
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Run one command on its explicitly selected authority.
    ///
    /// `command_route` is the issuing CLI's selected transport; `authority` is
    /// the authenticated server invocation that requested the command (for
    /// example the core CLI bridge service). At most one may be supplied: a
    /// command never merges two callers' authority, and without either it only
    /// has this instance's lifecycle authority.
    pub(in crate::runtime) fn run_command_with_dispatch(
        &self,
        command: &str,
        args: &[String],
        context: Option<&NativeCommandContext>,
        command_route: Option<&crate::runtime::plugin_kernel::CommandDispatch>,
        authority: Option<crate::runtime::plugin_kernel::AuthenticatedServiceInvocation>,
    ) -> bmux_plugin_sdk::Result<(i32, PluginCommandOutcome)> {
        if command_route.is_some() && authority.is_some() {
            return Err(failure(
                "plugin command has both a selected transport and a server authority",
            ));
        }
        match &self.backend {
            Backend::Process(plugin) => {
                // A manifest-selected out-of-process plugin receives host
                // calls through its process bridge, which forwards on the
                // service context it is run within; the authenticated server
                // invocation is that context. This is the selected backend,
                // not a fallback after a selected artifact failed.
                let run = || plugin.run_command_with_context_and_outcome(command, args, context);
                authority
                    .as_ref()
                    .map_or_else(run, |authority| authority.with_legacy_process_context(run))
            }
            Backend::Scoped(execution) => {
                let mut context = context
                    .cloned()
                    .ok_or_else(|| failure("scoped command context required"))?;
                self.bind_host_builtins(
                    &context.connection,
                    &context.services,
                    &context.plugin_settings_map,
                )?;
                context.host_kernel_bridge = None;
                if !self.active.load(std::sync::atomic::Ordering::Acquire) {
                    self.activate_scoped_command(execution, &context, command_route)?;
                }
                // A server-authenticated command inherits that invocation's
                // cancellation and callback ancestry, exactly like a provider
                // dispatched on the same authority; a CLI route supplies its own.
                let callback_parent = match command_route {
                    Some(route) => route.parent(),
                    None => authority
                        .as_ref()
                        .map(crate::runtime::plugin_kernel::AuthenticatedServiceInvocation::callback_parent)
                        .transpose()?
                        .flatten(),
                };
                let cancellation = authority.as_ref().map_or_else(
                    bmux_plugin_sdk::ServiceCancellation::new,
                    crate::runtime::plugin_kernel::AuthenticatedServiceInvocation::cancellation,
                );
                let invocation = execution.begin_invocation_scope()?;
                let runtime = host_runtime(execution)?;
                let binding = if let Some(parent) = callback_parent {
                    execution.bind_child_callback(&parent, invocation.clone(), cancellation)?
                } else {
                    execution.bind_root_callback(invocation.clone(), cancellation)?
                };
                let kernel = self
                    .kernel_grant
                    .as_ref()
                    .zip(authority.clone())
                    .map(|(grant, authority)| grant.bind_context(authority, binding.clone()));
                let dispatch = KernelServices {
                    declaration: self.declaration.clone(),
                    instance: execution.scope().id(),
                    authority: authority.or_else(|| self.lifecycle_authority.get().cloned()),
                    services: context.services.clone(),
                    builtins: Arc::clone(&self.builtins),
                    command_route: command_route.cloned(),
                    runtime,
                }
                .command_dispatch(&binding);
                let started = execution.begin_with_callback_dispatch(
                    NativeOperation::Command {
                        invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
                        context,
                    },
                    binding,
                    Some(dispatch),
                    kernel,
                );
                let id = match started {
                    Ok(id) => id,
                    Err(error) => {
                        invocation
                            .close()
                            .map_err(|cleanup| failure(cleanup.to_string()))?;
                        // Finish succeeds only when no accepted operation owns
                        // this scope; uncertain foreign work remains retained.
                        let _ = invocation.finish();
                        return Err(error);
                    }
                };
                match Self::observe_sync(execution, id)? {
                    NativeOutcome::Command(outcome) => Ok((
                        outcome.exit_code,
                        PluginCommandOutcome {
                            error_message: outcome.error,
                            metadata: outcome.metadata,
                        },
                    )),
                    _ => Err(failure("unexpected command outcome")),
                }
            }
        }
    }
    pub(in crate::runtime) fn invoke_service_sync(
        &self,
        context: &bmux_plugin_sdk::NativeServiceContext,
    ) -> bmux_plugin_sdk::Result<bmux_plugin_sdk::ServiceResponse> {
        match &self.backend {
            Backend::Process(plugin) => plugin.invoke_service(context),
            Backend::Scoped(execution) => {
                let mut context = context.clone();
                context.host_kernel_bridge = None;
                let id = execution.begin(NativeOperation::Invoke {
                    invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
                    context,
                })?;
                match Self::observe_sync(execution, id)? {
                    NativeOutcome::Service(response) => Ok(response),
                    _ => Err(failure("unexpected synchronous service outcome")),
                }
            }
        }
    }

    pub(in crate::runtime) fn invoke_service_authenticated_sync(
        &self,
        context: &bmux_plugin_sdk::NativeServiceContext,
        authority: crate::runtime::plugin_kernel::AuthenticatedServiceInvocation,
    ) -> bmux_plugin_sdk::Result<bmux_plugin_sdk::ServiceResponse> {
        let Backend::Scoped(execution) = &self.backend else {
            return authority.with_legacy_process_context(|| self.invoke_service_sync(context));
        };
        tracing::debug!(
            provider = self.declaration.id.as_str(),
            interface = context.request.service.interface_id.as_str(),
            operation = context.request.operation.as_str(),
            kernel_granted = self.kernel_grant.is_some(),
            "scoped authenticated provider dispatch"
        );
        let invocation = execution.begin_invocation_scope().map_err(|error| failure(format!(
            "selected provider invocation allocation failed: provider={} interface={} operation={} error={error}",
            self.declaration.id.as_str(), context.request.service.interface_id, context.request.operation,
        )))?;
        let cancellation = authority.cancellation();
        let callback_parent = authority.callback_parent()?;
        let kernel_authority = self.kernel_grant.as_ref().map(|_| authority.clone());
        let host = Arc::new(KernelServices {
            declaration: self.declaration.clone(),
            instance: execution.scope().id(),
            authority: Some(authority),
            services: context.services.clone(),
            builtins: Arc::clone(&self.builtins),
            command_route: None,
            runtime: host_runtime(execution)?,
        });
        let reentry = provider_reentry(&self.declaration, &context.request);
        let mut context = context.clone();
        context.host_kernel_bridge = None;
        context.host_service_port = None;
        let binding = if let Some(parent) = callback_parent {
            execution.bind_child_callback_with(
                &parent,
                invocation.clone(),
                cancellation,
                reentry,
            )?
        } else {
            execution.bind_root_callback(invocation.clone(), cancellation)?
        };
        // Kernel requests made inside the author entry carry that entry as
        // their call-chain parent, like the entered host route below.
        let kernel = self
            .kernel_grant
            .as_ref()
            .zip(kernel_authority)
            .map(|(grant, authority)| grant.bind_context(authority, binding.clone()));
        let host = binding.host_dispatch(Arc::new(move |entry| {
            Ok(Arc::new((host).with_callback_parent(entry)))
        }));
        let started = execution.begin_with_callback_dispatch(
            NativeOperation::Invoke {
                invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
                context,
            },
            binding,
            Some(host),
            kernel,
        );
        let id = match started {
            Ok(id) => id,
            Err(error) => {
                // Only unused scopes can finish here. If foreign start may
                // already own work, failed finish retains cleanup authority.
                invocation.close().map_err(|cleanup| {
                    failure(format!(
                        "operation admission failed: {error}; scope close failed: {cleanup}"
                    ))
                })?;
                invocation.finish().map_err(|cleanup| {
                    failure(format!(
                        "operation admission failed: {error}; scope cleanup retained: {cleanup}"
                    ))
                })?;
                return Err(error);
            }
        };
        match Self::observe_sync(execution, id)? {
            NativeOutcome::Service(response) => Ok(response),
            _ => Err(failure("unexpected authenticated service outcome")),
        }
    }

    pub(in crate::runtime) async fn invoke_service(
        &self,
        context: &bmux_plugin_sdk::NativeServiceContext,
        authority: crate::runtime::plugin_kernel::AuthenticatedServiceInvocation,
    ) -> anyhow::Result<bmux_plugin_sdk::ServiceResponse> {
        authority.dispatch(self.clone(), context.clone()).await
    }
}
