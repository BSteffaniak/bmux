//! Temporary scoped adapter for existing author implementations. It delegates
//! every hook to the same `RustPlugin` object; it never uses legacy global cells.
//! New implementations should implement `PluginInstance` directly.
use super::{
    CommandOutcome, ComponentRole, ContractDescriptor, PluginCallContext, PluginDefinition,
    PluginFuture, PluginInstance, PluginLifecycleContext, PluginMetadata, PluginResult,
};
use crate::{ContributionRegistrar, HostAsyncHandle, RustPlugin, TypedServiceRegistrationContext};
use crate::{
    NativeCommandContext, NativeServiceContext, NativeStreamingServiceContext, PluginCommandError,
    PluginEvent, ServiceResponse, TypedServiceRegistry,
};
use std::sync::{Arc, RwLock};

mod execution;
#[cfg(test)]
mod test_execution;

pub struct RustPluginAdapter<P>(
    Arc<RwLock<P>>,
    Arc<super::callback_gate::CallbackGate>,
    Option<super::LoadedInstanceId>,
);
impl<P> RustPluginAdapter<P> {
    /// Construct an unbound adapter for quiescent metadata inspection only.
    pub fn new(plugin: P) -> Self {
        Self(Arc::new(RwLock::new(plugin)), Arc::default(), None)
    }
    /// Bind host-issued instance correlation; actual calls still need authority.
    pub fn for_instance(plugin: P, instance: super::LoadedInstanceId) -> Self {
        Self(
            Arc::new(RwLock::new(plugin)),
            Arc::default(),
            Some(instance),
        )
    }
}
fn unavailable() -> PluginCommandError {
    PluginCommandError::unavailable("quiescent implementation lock unavailable or poisoned")
}
fn author_poisoned() -> PluginCommandError {
    PluginCommandError::failed("author implementation lock poisoned")
}
/// Hand the activation's host-supplied values to the legacy author, which
/// only sees the native lifecycle context.
fn with_activation_capabilities(
    mut native: crate::NativeLifecycleContext,
    capabilities: super::ActivationCapabilities,
) -> crate::NativeLifecycleContext {
    native.capabilities = capabilities;
    native
}
fn lifecycle_result(result: PluginResult<i32>) -> PluginResult<()> {
    match result {
        Ok(0) => Ok(()),
        Ok(code) => Err(PluginCommandError::new(
            code,
            "lifecycle hook returned failure",
        )),
        Err(error) => Err(error),
    }
}
// Legacy RustPlugin handlers own service bodies, unlike generated portable
// providers which own their entire checked envelope. Validate before obtaining
// the implementation lock or exposing a body to legacy code.
fn checked_context<C: crate::PluginContract>(
    mut context: NativeServiceContext,
) -> PluginResult<(NativeServiceContext, bool)> {
    let checked = crate::service_contract::is_checked(&context.request.payload);
    if checked {
        let interface = crate::service_contract::interface_id_for(
            &context.request.service.interface_id,
            &context.request.payload,
        )
        .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        let evidence = C::service_contract_for(interface).ok_or_else(|| {
            PluginCommandError::failed("provider has no generated contract for checked interface")
        })?;
        context.request.payload = crate::service_contract::decode_for(
            &context.request.service.interface_id,
            evidence,
            &context.request.payload,
        )
        .map_err(|error| PluginCommandError::failed(error.to_string()))?
        .to_vec();
    }
    Ok((context, checked))
}

fn checked_response(mut response: ServiceResponse, checked: bool) -> ServiceResponse {
    if checked && response.error.is_none() {
        response.payload = crate::service_contract::encode_response(&response.payload);
    }
    response
}

impl<P: RustPlugin + Sync> PluginInstance for RustPluginAdapter<P> {
    fn activate_scoped(
        &self,
        mut context: PluginLifecycleContext,
        services: Option<super::PluginHostServices>,
    ) -> PluginFuture<'_, PluginResult<()>> {
        if context.native.host_service_port.is_none() {
            context.native.host_service_port = services;
        }
        self.activate(context)
    }

    fn activate(&self, context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        let native = with_activation_capabilities(context.native, context.capabilities);
        self.schedule(context.callback_execution, true, move |state| {
            lifecycle_result(state.write().map_err(|_| unavailable())?.activate(native))
        })
    }
    fn activate_with_async(
        &self,
        context: PluginLifecycleContext,
        runtime: HostAsyncHandle,
    ) -> PluginFuture<'_, PluginResult<()>> {
        let native = with_activation_capabilities(context.native, context.capabilities);
        self.schedule(context.callback_execution, true, move |state| {
            lifecycle_result(
                state
                    .write()
                    .map_err(|_| unavailable())?
                    .activate_with_async(native, runtime),
            )
        })
    }
    fn deactivate(&self, context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        self.schedule(context.callback_execution, true, move |state| {
            lifecycle_result(
                state
                    .write()
                    .map_err(|_| unavailable())?
                    .deactivate(context.native),
            )
        })
    }
    fn command(
        &self,
        call: PluginCallContext,
        context: NativeCommandContext,
    ) -> PluginFuture<'_, CommandOutcome> {
        // The synchronous legacy body runs inside one bracketed capture, so
        // metadata it records returns as part of the outcome value.
        let future = self.schedule(call.callback_execution, true, move |state| {
            if call.cancellation.is_cancelled() {
                return Err(unavailable());
            }
            let mut plugin = state.write().map_err(|_| unavailable())?;
            Ok(CommandOutcome::capture(|| plugin.run_command(context)))
        });
        Box::pin(async move { future.await.unwrap_or_else(|error| Err(error).into()) })
    }
    fn invoke(
        &self,
        call: PluginCallContext,
        context: NativeServiceContext,
    ) -> PluginFuture<'_, PluginResult<ServiceResponse>> {
        self.schedule(call.callback_execution, false, move |state| {
            if call.cancellation.is_cancelled() {
                return Err(unavailable());
            }
            let (context, checked) = checked_context::<P::Contract>(context)?;
            let plugin = state.read().map_err(|_| unavailable())?;
            Ok(checked_response(plugin.invoke_service(context), checked))
        })
    }
    fn invoke_streaming(
        &self,
        call: PluginCallContext,
        context: NativeStreamingServiceContext,
    ) -> PluginFuture<'_, PluginResult<ServiceResponse>> {
        self.schedule(call.callback_execution, false, move |state| {
            if call.cancellation.is_cancelled() {
                return Err(unavailable());
            }
            let NativeStreamingServiceContext { service, events } = context;
            let (service, checked) = checked_context::<P::Contract>(service)?;
            let plugin = state.read().map_err(|_| unavailable())?;
            Ok(checked_response(
                plugin.invoke_streaming_service(NativeStreamingServiceContext { service, events }),
                checked,
            ))
        })
    }
    fn event_scoped(
        &self,
        call: PluginCallContext,
        event: PluginEvent,
    ) -> PluginFuture<'_, PluginResult<()>> {
        if !P::HANDLES_EVENTS {
            return Box::pin(async { Ok(()) });
        }
        self.schedule(call.callback_execution, true, move |state| {
            lifecycle_result(
                state
                    .write()
                    .map_err(|_| unavailable())?
                    .handle_event(event),
            )
        })
    }
    fn event(&self, _event: PluginEvent) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async {
            if P::HANDLES_EVENTS {
                Err(PluginCommandError::unavailable(
                    "event requires scoped callback authority",
                ))
            } else {
                Ok(())
            }
        })
    }
    fn register_typed_services_with_context(
        &self,
        context: TypedServiceRegistrationContext<'_>,
        registry: &mut TypedServiceRegistry,
    ) -> PluginResult<()> {
        self.0
            .try_read()
            .map_err(|_| unavailable())?
            .register_typed_services(context, registry);
        Ok(())
    }
    fn portable_services(&self) -> PluginResult<Vec<Arc<dyn crate::PortableServiceProvider>>> {
        self.0
            .try_read()
            .map_err(|_| unavailable())?
            .portable_services()
    }
    fn service_contract_for(&self, interface: &str) -> Option<&'static [u8]> {
        <P::Contract as crate::PluginContract>::service_contract_for(interface)
    }
    fn registration_contracts(&self) -> PluginResult<Vec<crate::RegistrationContract>> {
        self.0
            .try_read()
            .map_err(|_| unavailable())?
            .registration_contracts()
    }
    fn stateful_participants(&self) -> PluginResult<Vec<crate::StatefulPluginHandle>> {
        self.0
            .try_read()
            .map_err(|_| unavailable())?
            .stateful_participants()
    }
    fn register_contributions(&self, registrar: &mut ContributionRegistrar) -> PluginResult<()> {
        self.0
            .try_read()
            .map_err(|_| unavailable())?
            .register_contributions(registrar)
            .map_err(|error| PluginCommandError::failed(error.to_string()))
    }
}

fn legacy_factory<P: RustPlugin + Sync>(
    scope: &super::PluginCreateContext,
) -> Arc<dyn PluginInstance> {
    Arc::new(RustPluginAdapter::for_instance(
        P::default(),
        scope.instance,
    ))
}

// Routing aliases are not negotiation contracts. Only generated contract
// evidence AND a canonical explicitly versioned interface may add a descriptor.
fn embed_modeled_contracts<P: RustPlugin>(value: &mut toml::Value) -> PluginResult<()> {
    let mut modeled = Vec::new();
    for descriptor in <P::Contract as crate::PluginContract>::service_contract_descriptors() {
        // Generated enumeration includes unresolved imported wire closures.
        // Empty evidence is explicitly unavailable: retain routing declarations,
        // but never publish a negotiated identity or invent a digest for it.
        if super::ContractEvidence::from_generated(Some(descriptor.contract))?
            == super::ContractEvidence::Unavailable
        {
            continue;
        }
        descriptor
            .validate()
            .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        if <P::Contract as crate::PluginContract>::service_contract_for(descriptor.interface_id)
            != Some(descriptor.contract)
        {
            return Err(PluginCommandError::failed(
                "modeled descriptor conflicts with provider contract lookup",
            ));
        }
        let owned = super::DeclaredServiceContract {
            capability: descriptor.capability.into(),
            kind: descriptor.kind,
            interface_id: descriptor.interface_id.into(),
            contract: descriptor.contract.to_vec(),
        };
        owned.validate()?;
        modeled.push(owned);
    }
    let generated = toml::Value::try_from(&modeled)
        .map_err(|error| PluginCommandError::failed(error.to_string()))?;
    if let Some(explicit) = value.get("modeled_service_contracts")
        && explicit != &generated
    {
        return Err(PluginCommandError::failed(
            "modeled contracts conflict with generated evidence",
        ));
    }
    value
        .as_table_mut()
        .ok_or_else(|| PluginCommandError::failed("manifest must be a table"))?
        .insert("modeled_service_contracts".into(), generated);
    Ok(())
}

fn negotiation_contracts<P: RustPlugin>(
    manifest: &toml::Value,
    generated: &[crate::PluginService],
) -> PluginResult<Vec<ContractDescriptor>> {
    let mut contracts = manifest
        .get("contracts")
        .cloned()
        .map(toml::Value::try_into::<Vec<ContractDescriptor>>)
        .transpose()
        .map_err(|error| PluginCommandError::failed(error.to_string()))?
        .unwrap_or_default();
    validate_contracts(&contracts)?;
    for service in generated {
        let evidence = super::ContractEvidence::from_generated(
            <P::Contract as crate::PluginContract>::service_contract_for(&service.interface_id),
        )?;
        if evidence == super::ContractEvidence::Unavailable {
            // The generator deliberately emits empty evidence when wire
            // closure resolution is unavailable. Keep routing, not negotiation.
            continue;
        }
        let Some((_, version)) = service.interface_id.rsplit_once("/v") else {
            continue;
        };
        let major = version
            .parse::<u16>()
            .ok()
            .filter(|major| *major > 0 && major.to_string() == version)
            .ok_or_else(|| PluginCommandError::failed("invalid generated interface version"))?;
        let descriptor = ContractDescriptor {
            interface_id: service.interface_id.clone(),
            major,
            minor: 0,
        };
        if let Some(explicit) = contracts
            .iter()
            .find(|entry| entry.interface_id == descriptor.interface_id)
        {
            if explicit != &descriptor {
                return Err(PluginCommandError::failed(
                    "explicit contract conflicts with generated version evidence",
                ));
            }
        } else {
            contracts.push(descriptor);
        }
    }
    Ok(contracts)
}

pub(super) fn validate_contracts(contracts: &[ContractDescriptor]) -> PluginResult<()> {
    let mut ids = std::collections::BTreeSet::new();
    for contract in contracts {
        if contract.interface_id.trim().is_empty()
            || contract.major == 0
            || !ids.insert(&contract.interface_id)
        {
            return Err(PluginCommandError::failed(
                "empty, duplicate, or zero-version contract declaration",
            ));
        }
    }
    Ok(())
}

impl PluginDefinition {
    /// Adapt a legacy trait implementation with fresh per-create state. Parsing
    /// failure is explicit; malformed declarations never become empty plugins.
    /// # Errors
    /// Returns an error if the bound scope is unavailable, the request is invalid,
    /// or the implementation rejects the operation.
    pub fn from_rust_plugin<P: RustPlugin + Sync>(
        manifest: &str,
        roles: Vec<ComponentRole>,
    ) -> PluginResult<Self> {
        if roles.is_empty()
            || roles
                .iter()
                .enumerate()
                .any(|(index, role)| roles[..index].contains(role))
        {
            return Err(PluginCommandError::failed(
                "component roles must be explicit",
            ));
        }
        let mut value: toml::Value = toml::from_str(manifest)
            .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        let string = |key: &str| -> PluginResult<String> {
            value
                .get(key)
                .and_then(toml::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| PluginCommandError::failed(format!("missing plugin {key}")))
        };
        let plugin_id = string("id")?;
        let version = string("version")?;
        let services = P::declared_services()
            .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        let capabilities = value
            .get("required_capabilities")
            .and_then(toml::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        item.as_str().map(str::to_owned).ok_or_else(|| {
                            PluginCommandError::failed("invalid required capability")
                        })
                    })
                    .collect::<PluginResult<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        if let Some(declared_roles) = value.get("component_roles") {
            let declared_roles: Vec<ComponentRole> = declared_roles
                .clone()
                .try_into()
                .map_err(|error: toml::de::Error| PluginCommandError::failed(error.to_string()))?;
            if declared_roles != roles {
                return Err(PluginCommandError::failed(
                    "explicit component roles conflict with factory roles",
                ));
            }
        }
        embed_modeled_contracts::<P>(&mut value)?;
        let contracts = negotiation_contracts::<P>(&value, &services)?;
        // Embed generated declarations in the same manifest observed by static
        // discovery and the native descriptor. This is not a second service list
        // consumers must merge by implementation type.
        let mut declared = value
            .get("services")
            .cloned()
            .map(toml::Value::try_into::<Vec<crate::PluginService>>)
            .transpose()
            .map_err(|error| PluginCommandError::failed(error.to_string()))?
            .unwrap_or_default();
        for service in services {
            crate::merge_service_declaration(&mut declared, service);
        }
        value
            .as_table_mut()
            .ok_or_else(|| PluginCommandError::failed("manifest must be a table"))?
            .insert(
                "services".into(),
                toml::Value::try_from(&declared)
                    .map_err(|error| PluginCommandError::failed(error.to_string()))?,
            );
        let table = value
            .as_table_mut()
            .ok_or_else(|| PluginCommandError::failed("manifest must be a table"))?;
        table.insert(
            "component_roles".into(),
            toml::Value::try_from(&roles)
                .map_err(|error| PluginCommandError::failed(error.to_string()))?,
        );
        table.insert(
            "contracts".into(),
            toml::Value::try_from(&contracts)
                .map_err(|error| PluginCommandError::failed(error.to_string()))?,
        );
        let manifest = toml::to_string(&value)
            .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        let metadata = PluginMetadata {
            plugin_id,
            version,
            roles,
            required_capabilities: capabilities,
            contracts,
            manifest_toml: Some(manifest),
        };
        let definition = Self::new(metadata, |scope| Ok(legacy_factory::<P>(&scope)));
        definition.validate()?;
        Ok(definition)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{LoadedInstanceId, PluginCreateContext, RuntimeScopeId};
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Default)]
    struct Hooks {
        events: AtomicUsize,
    }
    impl RustPlugin for Hooks {
        type Contract = crate::NoPluginContract;
        fn run_command(&mut self, _context: NativeCommandContext) -> PluginResult<i32> {
            i32::try_from(self.events.load(Ordering::SeqCst))
                .map_err(|error| PluginCommandError::failed(error.to_string()))
        }
        fn handle_event(&mut self, _event: PluginEvent) -> PluginResult<i32> {
            self.events.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        }
        fn register_contributions(
            &self,
            registrar: &mut crate::ContributionRegistrar,
        ) -> crate::Result<()> {
            registrar
                .register(crate::PluginContribution::Extension {
                    id: "retained".into(),
                    extension_point: "example.surface/v1".into(),
                    payload: vec![1],
                })
                .map_err(|details| crate::PluginError::ServiceProtocol { details })
        }
    }
    const MANIFEST: &str = r#"
id = "example.hooks"
version = "1.0.0"
[[ready_signals]]
name = "initial-state"
[[extensions]]
extension_point = "example.frontend/v1"
payload = [1, 2]
"#;
    fn scope() -> PluginCreateContext {
        PluginCreateContext {
            runtime_scope: RuntimeScopeId::from_uuid(uuid::Uuid::from_u128(1)),
            instance: LoadedInstanceId::from_uuid(uuid::Uuid::from_u128(2)),
            role: ComponentRole::Frontend,
        }
    }
    #[test]
    fn embedded_declaration_retains_readiness_and_frontend_extensions() {
        let definition =
            PluginDefinition::from_rust_plugin::<Hooks>(MANIFEST, vec![ComponentRole::Frontend])
                .unwrap();
        let declarations = definition.declarations().unwrap();
        assert_eq!(declarations.ready_signals[0].name, "initial-state");
        assert_eq!(
            declarations.extensions[0].extension_point,
            "example.frontend/v1"
        );
        let instance = definition.create(scope()).unwrap();
        let registrations = instance.registrations().unwrap();
        assert_eq!(registrations.contributions.len(), 1);
    }
    #[test]
    fn migration_adapter_state_is_instance_owned_not_global() {
        let a = RustPluginAdapter::new(Hooks::default());
        let b = RustPluginAdapter::new(Hooks::default());
        a.0.write().unwrap().events.store(3, Ordering::SeqCst);
        assert_eq!(b.0.read().unwrap().events.load(Ordering::SeqCst), 0);
        let held = a.0.write().unwrap();
        // Registration must reject, not deadlock, when nested from a mutable hook.
        assert!(a.registrations().is_err());
        drop(held);
        assert!(a.registrations().is_ok());
    }
    #[test]
    fn invalid_manifest_or_missing_role_is_never_an_empty_installation() {
        assert!(
            PluginDefinition::from_rust_plugin::<Hooks>("{", vec![ComponentRole::Backend]).is_err()
        );
        assert!(
            PluginDefinition::from_rust_plugin::<Hooks>("id = 'x'", vec![ComponentRole::Backend])
                .is_err()
        );
        assert!(PluginDefinition::from_rust_plugin::<Hooks>(MANIFEST, vec![]).is_err());
    }
    const BUNDLED_MANIFESTS: &[(&str, &str)] = &[
        (
            "plugins/application-resurrection-plugin/plugin.toml",
            include_str!("../../../../plugins/application-resurrection-plugin/plugin.toml"),
        ),
        (
            "plugins/broadcast-plugin/plugin.toml",
            include_str!("../../../../plugins/broadcast-plugin/plugin.toml"),
        ),
        (
            "plugins/clients-plugin/plugin.toml",
            include_str!("../../../../plugins/clients-plugin/plugin.toml"),
        ),
        (
            "plugins/clipboard-plugin/plugin.toml",
            include_str!("../../../../plugins/clipboard-plugin/plugin.toml"),
        ),
        (
            "plugins/cluster-plugin/plugin.toml",
            include_str!("../../../../plugins/cluster-plugin/plugin.toml"),
        ),
        (
            "plugins/command-palette-plugin/plugin.toml",
            include_str!("../../../../plugins/command-palette-plugin/plugin.toml"),
        ),
        (
            "plugins/connections-plugin/plugin.toml",
            include_str!("../../../../plugins/connections-plugin/plugin.toml"),
        ),
        (
            "plugins/contexts-plugin/plugin.toml",
            include_str!("../../../../plugins/contexts-plugin/plugin.toml"),
        ),
        (
            "plugins/control-catalog-plugin/plugin.toml",
            include_str!("../../../../plugins/control-catalog-plugin/plugin.toml"),
        ),
        (
            "plugins/decoration-plugin/plugin.toml",
            include_str!("../../../../plugins/decoration-plugin/plugin.toml"),
        ),
        (
            "plugins/finder-plugin/plugin.toml",
            include_str!("../../../../plugins/finder-plugin/plugin.toml"),
        ),
        (
            "plugins/pane-runtime-plugin/plugin.toml",
            include_str!("../../../../plugins/pane-runtime-plugin/plugin.toml"),
        ),
        (
            "plugins/performance-plugin/plugin.toml",
            include_str!("../../../../plugins/performance-plugin/plugin.toml"),
        ),
        (
            "plugins/permissions-plugin/plugin.toml",
            include_str!("../../../../plugins/permissions-plugin/plugin.toml"),
        ),
        (
            "plugins/playbook-plugin/plugin.toml",
            include_str!("../../../../plugins/playbook-plugin/plugin.toml"),
        ),
        (
            "plugins/plugin-cli-plugin/plugin.toml",
            include_str!("../../../../plugins/plugin-cli-plugin/plugin.toml"),
        ),
        (
            "plugins/presenter-plugin/plugin.toml",
            include_str!("../../../../plugins/presenter-plugin/plugin.toml"),
        ),
        (
            "plugins/prompted-actions-plugin/plugin.toml",
            include_str!("../../../../plugins/prompted-actions-plugin/plugin.toml"),
        ),
        (
            "plugins/recording-plugin/plugin.toml",
            include_str!("../../../../plugins/recording-plugin/plugin.toml"),
        ),
        (
            "plugins/sessions-plugin/plugin.toml",
            include_str!("../../../../plugins/sessions-plugin/plugin.toml"),
        ),
        (
            "plugins/sidebar-plugin/plugin.toml",
            include_str!("../../../../plugins/sidebar-plugin/plugin.toml"),
        ),
        (
            "plugins/snapshot-plugin/plugin.toml",
            include_str!("../../../../plugins/snapshot-plugin/plugin.toml"),
        ),
        (
            "plugins/tab-bar-plugin/plugin.toml",
            include_str!("../../../../plugins/tab-bar-plugin/plugin.toml"),
        ),
        (
            "plugins/tabs-plugin/plugin.toml",
            include_str!("../../../../plugins/tabs-plugin/plugin.toml"),
        ),
        (
            "plugins/theme-plugin/plugin.toml",
            include_str!("../../../../plugins/theme-plugin/plugin.toml"),
        ),
        (
            "plugins/workspaces-plugin/plugin.toml",
            include_str!("../../../../plugins/workspaces-plugin/plugin.toml"),
        ),
    ];

    #[test]
    fn every_bundled_manifest_keeps_legacy_routes_without_inventing_contracts() {
        for (path, manifest) in BUNDLED_MANIFESTS {
            let input: toml::Value = toml::from_str(manifest).unwrap();
            let roles = input
                .get("component_roles")
                .cloned()
                .map(toml::Value::try_into::<Vec<ComponentRole>>)
                .transpose()
                .unwrap()
                .unwrap_or_else(|| vec![ComponentRole::Backend]);
            let definition = PluginDefinition::from_rust_plugin::<Hooks>(manifest, roles)
                .unwrap_or_else(|error| panic!("{path}: {error}"));
            definition.validate().unwrap();
            let original = input
                .get("services")
                .cloned()
                .map(toml::Value::try_into::<Vec<crate::PluginService>>)
                .transpose()
                .unwrap()
                .unwrap_or_default();
            assert_eq!(
                definition.declarations().unwrap().services,
                original,
                "{path}"
            );
            let explicit = input
                .get("contracts")
                .cloned()
                .map(toml::Value::try_into::<Vec<ContractDescriptor>>)
                .transpose()
                .unwrap()
                .unwrap_or_default();
            assert_eq!(definition.metadata.contracts, explicit, "{path}");
        }
    }
    #[test]
    fn invalid_explicit_contracts_are_rejected_not_rewritten() {
        for contracts in [
            "[[contracts]]\ninterface_id = 'example/v1'\nmajor = 0\nminor = 0\n",
            "[[contracts]]\ninterface_id = ''\nmajor = 1\nminor = 0\n",
            "[[contracts]]\ninterface_id = 'example/v1'\nmajor = 1\nminor = 0\n[[contracts]]\ninterface_id = 'example/v1'\nmajor = 1\nminor = 0\n",
            "contracts = 'invalid'\n",
        ] {
            let manifest = format!("id = 'example.invalid'\nversion = '1'\n{contracts}");
            assert!(
                PluginDefinition::from_rust_plugin::<Hooks>(
                    &manifest,
                    vec![ComponentRole::Backend]
                )
                .is_err()
            );
        }
    }
    #[test]
    fn embedded_identity_mismatch_fails_definition_validation() {
        let mut definition =
            PluginDefinition::from_rust_plugin::<Hooks>(MANIFEST, vec![ComponentRole::Frontend])
                .unwrap();
        definition.metadata.roles = vec![ComponentRole::Backend];
        assert!(definition.validate().is_err());
        definition.metadata.roles = vec![ComponentRole::Frontend];
        definition.metadata.contracts.push(ContractDescriptor {
            interface_id: "invented/v1".into(),
            major: 1,
            minor: 0,
        });
        assert!(definition.validate().is_err());
    }
    struct GeneratedContract;
    impl crate::PluginContract for GeneratedContract {
        fn service_contract_descriptors()
        -> &'static [crate::typed_dispatch::ModeledServiceContractDescriptor] {
            &[
                crate::typed_dispatch::ModeledServiceContractDescriptor {
                    capability: "example.read",
                    kind: crate::ServiceKind::Query,
                    interface_id: "unresolved/v1",
                    contract: &[],
                },
                crate::typed_dispatch::ModeledServiceContractDescriptor {
                    capability: "example.read",
                    kind: crate::ServiceKind::Query,
                    interface_id: "generated/v3",
                    contract: &[1; 32],
                },
            ]
        }
        fn service_contract_for(interface: &str) -> Option<&'static [u8]> {
            match interface {
                "unresolved/v1" => Some(&[]),
                "generated/v3" | "generated-alias-v1" => Some(&[1; 32]),
                _ => None,
            }
        }
        fn service_declarations() -> crate::Result<Vec<crate::PluginService>> {
            [
                "clients-state",
                "generated-alias-v1",
                "generated/v3",
                "unresolved/v1",
            ]
            .into_iter()
            .map(|interface| {
                Ok(crate::PluginService {
                    capability: crate::HostScope::new("example.read")?,
                    kind: crate::ServiceKind::Query,
                    interface_id: interface.into(),
                    concurrency: None,
                    reentrant_operations: std::collections::BTreeSet::new(),
                })
            })
            .collect()
        }
    }
    struct MalformedContract;
    impl crate::PluginContract for MalformedContract {
        fn service_contract_descriptors()
        -> &'static [crate::typed_dispatch::ModeledServiceContractDescriptor] {
            &[crate::typed_dispatch::ModeledServiceContractDescriptor {
                capability: "example.read",
                kind: crate::ServiceKind::Query,
                interface_id: "malformed/v1",
                contract: &[1; 31],
            }]
        }
        fn service_declarations() -> crate::Result<Vec<crate::PluginService>> {
            Ok(vec![])
        }
    }
    #[derive(Default)]
    struct MalformedHooks;
    impl RustPlugin for MalformedHooks {
        type Contract = MalformedContract;
    }
    #[test]
    fn nonempty_malformed_modeled_evidence_is_not_treated_as_unavailable() {
        assert!(
            PluginDefinition::from_rust_plugin::<MalformedHooks>(
                MANIFEST,
                vec![ComponentRole::Frontend]
            )
            .is_err()
        );
    }
    #[derive(Default)]
    struct GeneratedHooks;
    impl RustPlugin for GeneratedHooks {
        type Contract = GeneratedContract;
    }
    #[test]
    fn generated_evidence_does_not_assign_versions_to_aliases() {
        let definition = PluginDefinition::from_rust_plugin::<GeneratedHooks>(
            MANIFEST,
            vec![ComponentRole::Frontend],
        )
        .unwrap();
        assert_eq!(definition.declarations().unwrap().services.len(), 4);
        let modeled = definition.declarations().unwrap().modeled_service_contracts;
        assert_eq!(modeled.len(), 1);
        assert_eq!(modeled[0].interface_id, "generated/v3");
        assert_eq!(modeled[0].capability, "example.read");
        assert_eq!(modeled[0].contract, vec![1; 32]);
        let conflicting_evidence = format!(
            "{MANIFEST}\n[[modeled_service_contracts]]\ncapability = 'example.read'\nkind = 'query'\ninterface_id = 'generated-alias-v1'\ncontract = [1]\n"
        );
        assert!(
            PluginDefinition::from_rust_plugin::<GeneratedHooks>(
                &conflicting_evidence,
                vec![ComponentRole::Frontend]
            )
            .is_err()
        );
        assert_eq!(
            definition.metadata.contracts,
            vec![ContractDescriptor {
                interface_id: "generated/v3".into(),
                major: 3,
                minor: 0
            }]
        );
        definition.validate().unwrap();
        let conflicting = format!(
            "{MANIFEST}\n[[contracts]]\ninterface_id = 'generated/v3'\nmajor = 2\nminor = 0\n"
        );
        assert!(
            PluginDefinition::from_rust_plugin::<GeneratedHooks>(
                &conflicting,
                vec![ComponentRole::Frontend]
            )
            .is_err()
        );
    }
    #[derive(Default)]
    struct BodyProvider;
    impl RustPlugin for BodyProvider {
        type Contract = GeneratedContract;
        fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
            assert_eq!(context.request.payload, b"body");
            if context.request.operation == "fail" {
                ServiceResponse::error("test_error", "preserve service error")
            } else {
                ServiceResponse::ok(b"reply".to_vec())
            }
        }
        fn invoke_streaming_service(
            &self,
            context: NativeStreamingServiceContext,
        ) -> ServiceResponse {
            context
                .events
                .emit_event(crate::ServiceEvent {
                    invocation_id: context.service.cancellation.invocation_id.clone(),
                    payload: b"event".to_vec(),
                })
                .unwrap();
            self.invoke_service(context.service)
        }
    }
    fn body_context(payload: Vec<u8>) -> NativeServiceContext {
        NativeServiceContext {
            plugin_id: "example.body".into(),
            request: crate::ServiceRequest {
                caller_plugin_id: "example.caller".into(),
                service: crate::RegisteredService {
                    capability: crate::HostScope::new("example.read").unwrap(),
                    kind: crate::ServiceKind::Query,
                    interface_id: "generated/v3".into(),
                    provider: crate::ProviderId::Plugin("example.body".into()),
                },
                operation: "read".into(),
                payload,
            },
            required_capabilities: vec![],
            provided_capabilities: vec![],
            services: vec![],
            available_capabilities: vec![],
            enabled_plugins: vec![],
            plugin_search_roots: vec![],
            host: crate::HostMetadata {
                product_name: "test".into(),
                product_version: "1".into(),
                plugin_api_version: crate::ApiVersion::new(1, 0),
                plugin_abi_version: crate::ApiVersion::new(2, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: String::new(),
                config_dir_candidates: vec![],
                runtime_dir: String::new(),
                data_dir: String::new(),
                state_dir: String::new(),
            },
            settings: None,
            plugin_settings_map: std::collections::BTreeMap::new(),
            caller_client_id: None,
            cancellation: crate::CancellationToken::new(),
            host_kernel_bridge: None,
            host_service_port: None,
        }
    }
    fn body_call(context: &NativeServiceContext) -> PluginCallContext {
        PluginCallContext {
            callback_execution: Some(test_execution::context()),
            invocation_id: context.cancellation.invocation_id.clone(),
            cancellation: crate::ServiceCancellation::new(),
            host_services: None,
        }
    }
    #[tokio::test]
    async fn legacy_checked_body_is_validated_and_response_wrapped_once() {
        let provider = RustPluginAdapter::for_instance(BodyProvider, scope().instance);
        let frame = crate::service_contract::encode("generated/v3", &[1; 32], b"body").unwrap();
        let context = body_context(frame.clone());
        let response = provider.invoke(body_call(&context), context).await.unwrap();
        assert_eq!(
            response.payload,
            crate::service_contract::encode_response(b"reply")
        );
        let legacy = body_context(b"body".to_vec());
        assert_eq!(
            provider
                .invoke(body_call(&legacy), legacy)
                .await
                .unwrap()
                .payload,
            b"reply"
        );
        let mut failing = body_context(frame);
        failing.request.operation = "fail".into();
        let response = provider.invoke(body_call(&failing), failing).await.unwrap();
        assert_eq!(response.error.unwrap().code, "test_error");
        assert_eq!(response.payload, [] as [u8; 0]);
    }
    #[tokio::test]
    async fn legacy_checked_wrong_route_missing_contract_and_wrong_evidence_reject_before_body() {
        let provider = RustPluginAdapter::for_instance(BodyProvider, scope().instance);
        for (interface, digest) in [("other/v3", [1; 32]), ("generated/v3", [2; 32])] {
            let context = body_context(
                crate::service_contract::encode(interface, &digest, b"not body").unwrap(),
            );
            assert!(provider.invoke(body_call(&context), context).await.is_err());
        }
        let mut missing = body_context(
            crate::service_contract::encode("missing/v1", &[1; 32], b"not body").unwrap(),
        );
        missing.request.service.interface_id = "missing/v1".into();
        assert!(provider.invoke(body_call(&missing), missing).await.is_err());
    }
    #[derive(Clone)]
    struct CaptureEvents(Arc<std::sync::Mutex<Vec<crate::ServiceEvent>>>);
    impl crate::ServiceEventSink for CaptureEvents {
        fn emit_event(&self, event: crate::ServiceEvent) -> crate::Result<()> {
            self.0.lock().unwrap().push(event);
            Ok(())
        }
    }
    #[tokio::test]
    async fn legacy_checked_stream_keeps_sink_and_live_cancellation() {
        let provider = RustPluginAdapter::for_instance(BodyProvider, scope().instance);
        let context = body_context(
            crate::service_contract::encode("generated/v3", &[1; 32], b"body").unwrap(),
        );
        let call = body_call(&context);
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stream = NativeStreamingServiceContext {
            service: context,
            events: crate::ServiceEventSinkHandle::new(CaptureEvents(Arc::clone(&captured))),
        };
        let response = provider
            .invoke_streaming(call.clone(), stream.clone())
            .await
            .unwrap();
        assert_eq!(
            response.payload,
            crate::service_contract::encode_response(b"reply")
        );
        assert_eq!(captured.lock().unwrap().len(), 1);
        call.cancellation.cancel();
        assert!(provider.invoke_streaming(call, stream).await.is_err());
        assert_eq!(captured.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn absent_callback_authority_fails_before_legacy_author_body() {
        let provider = RustPluginAdapter::for_instance(BodyProvider, scope().instance);
        let context = body_context(b"body".to_vec());
        let mut call = body_call(&context);
        call.callback_execution = None;
        assert!(provider.invoke(call, context).await.is_err());
    }

    #[tokio::test]
    async fn unbound_adapter_is_metadata_only_even_with_selected_execution() {
        let provider = RustPluginAdapter::new(BodyProvider);
        let context = body_context(b"body".to_vec());
        assert!(provider.invoke(body_call(&context), context).await.is_err());
    }
}
