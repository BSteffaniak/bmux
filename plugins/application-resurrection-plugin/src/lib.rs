//! Plugin-owned application resurrection lifecycle and local host transport.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]

pub mod execution;
#[cfg(unix)]
mod host;
#[cfg(unix)]
mod process;
mod recovery;
/// Local shell lifecycle callbacks using the ordinary BMUX executable.
#[cfg(unix)]
pub mod shell;

#[cfg(unix)]
use bmux_application_resurrection_plugin_api::{resurrection_host_v1, resurrection_launch_v1};
use bmux_plugin_sdk::{
    NativeLifecycleContext, NativeServiceContext, PluginCommandError, RustPlugin, ServiceResponse,
};

/// Statically bundleable, independently disableable host adapter.
#[derive(Default)]
pub struct ApplicationResurrectionPlugin {
    #[cfg(unix)]
    host: std::sync::RwLock<Option<host::Host>>,
}

impl RustPlugin for ApplicationResurrectionPlugin {
    type Contract = bmux_application_resurrection_plugin_api::Contract;

    fn activate(&mut self, context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        self.activate_host(&context)?;
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn deactivate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        self.deactivate_host()?;
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        self.service(&context)
    }
}

impl ApplicationResurrectionPlugin {
    fn activate_host(&self, context: &NativeLifecycleContext) -> Result<(), PluginCommandError> {
        #[cfg(unix)]
        {
            let mut host = self
                .host
                .write()
                .map_err(|_| PluginCommandError::failed("resurrection host lock poisoned"))?;
            if host.is_some() {
                return Err(PluginCommandError::failed(
                    "resurrection host already active",
                ));
            }
            *host = Some(
                host::Host::new(
                    std::path::PathBuf::from(&context.connection.state_dir)
                        .join("application-resurrection"),
                )
                .map_err(PluginCommandError::failed)?,
            );
        }
        #[cfg(not(unix))]
        let _ = context;
        Ok(())
    }

    fn deactivate_host(&self) -> Result<(), PluginCommandError> {
        #[cfg(unix)]
        {
            let mut host = self
                .host
                .write()
                .map_err(|_| PluginCommandError::failed("resurrection host lock poisoned"))?;
            if let Some(active) = host.as_ref() {
                active.shutdown().map_err(PluginCommandError::failed)?;
            }
            *host = None;
        }
        Ok(())
    }

    fn service(&self, context: &NativeServiceContext) -> ServiceResponse {
        #[cfg(unix)]
        {
            let Ok(host) = self.host.read() else {
                return ServiceResponse::error(
                    "host_unavailable",
                    "resurrection host lock poisoned",
                );
            };
            let host = host.as_ref();
            let unavailable = || resurrection_launch_v1::HostError::Failed {
                message: "host unavailable".into(),
            };
            bmux_plugin_sdk::route_service!(context, {
                resurrection_host_v1::client::StatusEndpoint => |_request, _context| {
                    Ok(resurrection_host_v1::HostStatus { protocol: "application-resurrection/1".into(), available: host.is_some(), reason: None })
                },
                resurrection_launch_v1::client::PrepareEndpoint => |request, _context| {
                    Ok(host.ok_or_else(unavailable).and_then(|host| host.prepare(&request.key, request.restored, request.shell, request.replay).map_err(host_error)))
                },
                bmux_application_resurrection_plugin_api::resurrection_launch_v2::client::PrepareEndpoint => |request, _context| {
                    Ok(host.ok_or_else(unavailable).and_then(|host| host.prepare_checked(&request.key, request.restored, request.shell, request.replay, host::IdentityCheck::Expected(request.expected_invocation.as_deref())).map_err(host_error)))
                },
                bmux_application_resurrection_plugin_api::resurrection_launch_v2::client::BoundaryEndpoint => |request, _context| {
                    Ok(host.ok_or_else(unavailable).and_then(|host| host.boundary_identity(&request.key, &request.endpoint, request.prompt).map_err(host_error)))
                },
                bmux_application_resurrection_plugin_api::resurrection_launch_v2::client::TakeReplayEndpoint => |request, _context| {
                    Ok(host.ok_or_else(unavailable).and_then(|host| host.take_replay(&request.key, &request.endpoint).map_err(host_error)))
                },
                resurrection_launch_v1::client::BindEndpoint => |request, _context| {
                    Ok(host.ok_or_else(unavailable).and_then(|host| host.bind(&request.key, &request.endpoint, request.pid).map_err(host_error)))
                },
                resurrection_launch_v1::client::BoundaryEndpoint => |request, _context| {
                    Ok(host.ok_or_else(unavailable).and_then(|host| host.boundary(&request.key, &request.endpoint, request.prompt).map_err(host_error)))
                },
                resurrection_launch_v1::client::CloseEndpoint => |request, _context| {
                    Ok(host.ok_or_else(unavailable).and_then(|host| host.close(&request.key, &request.endpoint).map_err(host_error)))
                },
            })
        }
        #[cfg(not(unix))]
        {
            let _ = context;
            ServiceResponse::error("unsupported", "local transport unavailable")
        }
    }
}

#[cfg(unix)]
fn host_error(message: String) -> resurrection_launch_v1::HostError {
    resurrection_launch_v1::HostError::Failed { message }
}

impl bmux_plugin_sdk::definition::PluginInstance for ApplicationResurrectionPlugin {
    fn service_contract_for(&self, interface: &str) -> Option<&'static [u8]> {
        <bmux_application_resurrection_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface)
    }

    fn activate(
        &self,
        context: bmux_plugin_sdk::definition::PluginLifecycleContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'_, bmux_plugin_sdk::definition::PluginResult<()>>
    {
        Box::pin(async move { self.activate_host(&context.native) })
    }

    fn deactivate(
        &self,
        _context: bmux_plugin_sdk::definition::PluginLifecycleContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'_, bmux_plugin_sdk::definition::PluginResult<()>>
    {
        Box::pin(async move { self.deactivate_host() })
    }

    fn invoke(
        &self,
        call: bmux_plugin_sdk::definition::PluginCallContext,
        context: NativeServiceContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        '_,
        bmux_plugin_sdk::definition::PluginResult<ServiceResponse>,
    > {
        Box::pin(async move {
            if call.cancellation.is_cancelled() {
                return Err(PluginCommandError::unavailable(
                    "resurrection invocation cancelled",
                ));
            }
            bmux_plugin_sdk::typed_dispatch::invoke_checked_service_body::<
                bmux_application_resurrection_plugin_api::Contract,
            >(context, |context| self.service(&context))
            .map_err(|error| PluginCommandError::failed(error.to_string()))
        })
    }
}

bmux_plugin_sdk::define_plugin!(
    factory = |_scope| Ok(
        std::sync::Arc::new(ApplicationResurrectionPlugin::default())
            as std::sync::Arc<dyn bmux_plugin_sdk::definition::PluginInstance>
    ),
    metadata = bmux_plugin_sdk::definition::PluginDefinition::from_rust_plugin::<
        ApplicationResurrectionPlugin,
    >(
        include_str!("../plugin.toml"),
        vec![bmux_plugin_sdk::definition::ComponentRole::Backend],
    )
    .expect("resurrection declaration must be valid")
    .metadata,
);

#[cfg(all(test, unix))]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn embedded_manifest_matches_definition_metadata() {
        plugin_definition()
            .validate()
            .expect("resurrection embedded identity must validate");
    }

    #[test]
    fn selected_instance_preserves_generated_contracts_for_checked_dispatch() {
        use bmux_plugin_sdk::definition::PluginInstance;

        let plugin = ApplicationResurrectionPlugin::default();
        for interface in [
            "resurrection-host-v1",
            "resurrection-launch-v1",
            "resurrection-launch-v2",
        ] {
            let generated = <bmux_application_resurrection_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for(interface)
                .expect("declared interface must have generated evidence");
            let selected = plugin
                .service_contract_for(interface)
                .expect("selected implementation must expose generated evidence");
            assert_eq!(selected, generated);
            let checked =
                bmux_plugin_sdk::service_contract::encode(interface, generated, b"request")
                    .unwrap();
            assert_eq!(
                bmux_plugin_sdk::service_contract::decode_for(interface, selected, &checked)
                    .unwrap(),
                b"request"
            );
            assert!(
                bmux_plugin_sdk::service_contract::decode_for(
                    "unrelated-interface",
                    selected,
                    &checked,
                )
                .is_err()
            );
        }
        assert!(plugin.service_contract_for("unrelated-interface").is_none());
    }

    #[test]
    fn selected_prepare_decodes_one_checked_request_and_frames_one_response() {
        use bmux_plugin_sdk::definition::{PluginCallContext, PluginInstance};
        use bmux_plugin_sdk::{HostScope, ServiceKind, TypedServiceEndpoint};
        type Endpoint = bmux_application_resurrection_plugin_api::resurrection_launch_v2::client::PrepareEndpoint;

        let request: <Endpoint as TypedServiceEndpoint>::Request = serde_json::from_value(
            serde_json::json!({"key": "isolated-test", "restored": false, "shell": false,
                "replay": false, "expected_invocation": null}),
        )
        .unwrap();
        let body = bmux_plugin_sdk::encode_service_message(&request).unwrap();
        let evidence = <bmux_application_resurrection_plugin_api::Contract as bmux_plugin_sdk::PluginContract>::service_contract_for("resurrection-launch-v2").unwrap();
        let checked =
            bmux_plugin_sdk::service_contract::encode("resurrection-launch-v2", evidence, &body)
                .unwrap();
        let context = bmux_plugin_sdk::NativeServiceContext {
            plugin_id: "bmux.application_resurrection".into(),
            request: bmux_plugin_sdk::ServiceRequest {
                caller_plugin_id: "example.caller".into(),
                service: bmux_plugin_sdk::RegisteredService {
                    capability: HostScope::new("bmux.application_resurrection.host").unwrap(),
                    kind: ServiceKind::Command,
                    interface_id: "resurrection-launch-v2".into(),
                    provider: bmux_plugin_sdk::ProviderId::Plugin(
                        "bmux.application_resurrection".into(),
                    ),
                },
                operation: "prepare".into(),
                payload: checked,
            },
            required_capabilities: vec![],
            provided_capabilities: vec![],
            services: vec![],
            available_capabilities: vec![],
            enabled_plugins: vec![],
            plugin_search_roots: vec![],
            host: bmux_plugin_sdk::HostMetadata {
                product_name: "test".into(),
                product_version: "1".into(),
                plugin_api_version: bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION,
                plugin_abi_version: bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION,
            },
            connection: bmux_plugin_sdk::HostConnectionInfo {
                config_dir: String::new(),
                config_dir_candidates: vec![],
                runtime_dir: String::new(),
                data_dir: String::new(),
                state_dir: String::new(),
            },
            settings: None,
            plugin_settings_map: std::collections::BTreeMap::default(),
            caller_client_id: None,
            cancellation: bmux_plugin_sdk::CancellationToken::new(),
            host_kernel_bridge: None,
            host_service_port: None,
        };
        let plugin = ApplicationResurrectionPlugin::default();
        let call = PluginCallContext {
            callback_execution: None,
            invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
            cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
            host_services: None,
        };
        let mut future = plugin.invoke(call, context);
        let std::task::Poll::Ready(result) = future
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        else {
            panic!("inactive host prepare must settle without I/O")
        };
        let response = result.unwrap();
        assert!(response.error.is_none());
        let body = bmux_plugin_sdk::service_contract::decode_response(&response.payload).unwrap();
        assert!(bmux_plugin_sdk::service_contract::decode_response(body).is_err());
        let result: <Endpoint as TypedServiceEndpoint>::Response =
            bmux_plugin_sdk::decode_service_message(body).unwrap();
        assert!(
            matches!(result, Err(resurrection_launch_v1::HostError::Failed { message }) if message == "host unavailable")
        );
        assert!(plugin.host.read().unwrap().is_none());
    }

    #[test]
    fn fresh_instances_own_separate_host_slots() {
        let first = ApplicationResurrectionPlugin::default();
        let second = ApplicationResurrectionPlugin::default();
        assert!(first.host.read().unwrap().is_none());
        assert!(second.host.read().unwrap().is_none());
        first.deactivate_host().unwrap();
        second.deactivate_host().unwrap();
    }
}
