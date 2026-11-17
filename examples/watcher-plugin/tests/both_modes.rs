//! The watcher's contract test, run in both packaging modes.
//!
//! Bundled: the counter registers its generated typed providers and the
//! watcher calls them directly, with no serialization. Dylib: the watcher's
//! generated client encodes checked frames that reach the counter through
//! `PluginInstance::invoke`, exactly as the host forwards a call across the
//! dylib boundary. Both must agree.

use std::collections::BTreeMap;
use std::sync::Arc;

use bmux_example_counter_plugin::CounterPlugin;
use bmux_example_watcher_plugin::declaration::requires::{counter_commands, counter_state};
use bmux_example_watcher_plugin::{WatcherPlugin, watch};
use bmux_plugin_sdk::author::{CommandArgs, RequiredInterface};
use bmux_plugin_sdk::definition::{PluginCallContext, PluginInstance};
use bmux_plugin_sdk::{
    ApiVersion, CancellationToken, HostConnectionInfo, HostMetadata, HostScope,
    NativeServiceContext, PluginInvocationId, ProviderId, RegisteredService, ServiceCancellation,
    ServiceKind, ServiceRequest, TypedDispatchClient, TypedDispatchClientError,
    TypedServiceRegistry,
};

/// Forwards encoded calls into a provider instance's `invoke`, as the host
/// does for a dylib.
#[derive(Clone)]
struct HostLoopback(Arc<CounterPlugin>);

impl TypedDispatchClient for HostLoopback {
    fn requires_service_contracts(&self) -> bool {
        true
    }

    async fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: bmux_ipc::InvokeServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, TypedDispatchClientError> {
        let kind = match kind {
            bmux_ipc::InvokeServiceKind::Query => ServiceKind::Query,
            bmux_ipc::InvokeServiceKind::Command => ServiceKind::Command,
        };
        let context = service_context(capability, kind, interface_id, operation, payload);
        let call = PluginCallContext {
            callback_execution: None,
            invocation_id: PluginInvocationId::new(),
            cancellation: ServiceCancellation::new(),
            host_services: None,
        };
        let response = self.0.invoke(call, context).await.map_err(|error| {
            TypedDispatchClientError::Transport {
                interface: interface_id.to_owned(),
                operation: operation.to_owned(),
                details: error.message,
            }
        })?;
        match response.error {
            Some(error) => Err(TypedDispatchClientError::Server {
                interface: interface_id.to_owned(),
                operation: operation.to_owned(),
                details: error.message,
            }),
            None => Ok(response.payload),
        }
    }
}

fn service_context(
    capability: &str,
    kind: ServiceKind,
    interface_id: &str,
    operation: &str,
    payload: Vec<u8>,
) -> NativeServiceContext {
    NativeServiceContext {
        plugin_id: "example.counter".into(),
        request: ServiceRequest {
            caller_plugin_id: "example.watcher".into(),
            service: RegisteredService {
                capability: HostScope::new(capability).unwrap(),
                kind,
                interface_id: interface_id.into(),
                provider: ProviderId::Plugin("example.counter".into()),
            },
            operation: operation.into(),
            payload,
        },
        required_capabilities: vec![],
        provided_capabilities: vec![],
        services: vec![],
        available_capabilities: vec![],
        enabled_plugins: vec![],
        plugin_search_roots: vec![],
        host: HostMetadata {
            product_name: "test".into(),
            product_version: "1".into(),
            plugin_api_version: ApiVersion::new(1, 0),
            plugin_abi_version: ApiVersion::new(2, 0),
        },
        connection: HostConnectionInfo {
            config_dir: String::new(),
            config_dir_candidates: vec![],
            runtime_dir: String::new(),
            data_dir: String::new(),
            state_dir: String::new(),
        },
        settings: None,
        plugin_settings_map: BTreeMap::new(),
        caller_client_id: None,
        cancellation: CancellationToken::new(),
        host_kernel_bridge: None,
        host_service_port: None,
    }
}

fn handle<R: RequiredInterface>(
    registry: &TypedServiceRegistry,
) -> &bmux_plugin_sdk::TypedServiceHandle {
    let descriptor = R::DESCRIPTOR;
    registry
        .get(
            &HostScope::new(descriptor.capability.as_str()).unwrap(),
            descriptor.kind,
            descriptor.interface_id.as_str(),
        )
        .expect("counter registers every provided interface")
}

#[tokio::test]
async fn watcher_sees_the_same_counter_bundled_and_through_the_dylib_codec() {
    // Bundled: direct typed calls through the registry.
    let bundled = CounterPlugin::default();
    let mut registry = TypedServiceRegistry::new();
    bundled.register_typed_services(&mut registry).unwrap();
    let commands = counter_commands::v1::Client::<HostLoopback>::from_handle(handle::<
        counter_commands::v1::Client,
    >(&registry))
    .unwrap();
    let state = counter_state::v1::Client::<HostLoopback>::from_handle(handle::<
        counter_state::v1::Client,
    >(&registry))
    .unwrap();
    let direct = [
        watch(&commands, &state, 2).await.unwrap(),
        watch(&commands, &state, 3).await.unwrap(),
    ];

    // Dylib: checked frames through `PluginInstance::invoke`.
    let host = HostLoopback(Arc::new(CounterPlugin::default()));
    let commands = counter_commands::v1::Client::remote(host.clone());
    let state = counter_state::v1::Client::remote(host);
    let codec = [
        watch(&commands, &state, 2).await.unwrap(),
        watch(&commands, &state, 3).await.unwrap(),
    ];

    assert_eq!(direct, [(2, 2), (5, 5)]);
    assert_eq!(direct, codec);
}

#[tokio::test]
async fn provider_failures_reach_the_consumer_in_both_modes() {
    let host = HostLoopback(Arc::new(CounterPlugin::default()));
    let commands = counter_commands::v1::Client::remote(host);
    commands.incr(u64::MAX).await.unwrap();
    let error = commands.incr(1).await.unwrap_err();
    assert!(error.message.contains("counter overflow"), "{error:?}");

    let bundled = CounterPlugin::default();
    let mut registry = TypedServiceRegistry::new();
    bundled.register_typed_services(&mut registry).unwrap();
    let commands = counter_commands::v1::Client::<HostLoopback>::from_handle(handle::<
        counter_commands::v1::Client,
    >(&registry))
    .unwrap();
    commands.incr(u64::MAX).await.unwrap();
    assert!(
        commands
            .incr(1)
            .await
            .unwrap_err()
            .message
            .contains("counter overflow")
    );
}

#[test]
fn watcher_declares_only_what_it_requires() {
    let definition = bmux_example_watcher_plugin::plugin_definition();
    definition
        .validate()
        .expect("embedded manifest matches metadata");
    let manifest = definition.metadata.manifest_toml.as_deref().unwrap();
    assert!(manifest.contains("example.counter.write"));
    assert!(manifest.contains("example.counter.read"));
    assert!(manifest.contains("name = \"watch\""));
    let args = bmux_example_watcher_plugin::declaration::commands::WatchArgs::parse(&[
        "--by".to_owned(),
        "4".to_owned(),
    ])
    .unwrap();
    assert_eq!(args.by, Some(4));
    let _ = WatcherPlugin;
}
