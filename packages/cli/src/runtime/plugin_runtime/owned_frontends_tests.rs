use super::*;
use crate::runtime::plugin_runtime::{
    SelectedCommandStateGuard, build_runtime_command_state, load_plugin, plugin_command_context,
    plugin_host_metadata, register_bundled_definition,
};
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_plugin_runtime::ActivationScope;
use bmux_plugin_sdk::definition::{
    ActivationCapabilities, CommandOutcome, ComponentRole, PluginCallContext, PluginDefinition,
    PluginFuture, PluginInstance, PluginLifecycleContext, PluginMetadata, PluginResult,
};

/// An attach-side value minted from one frontend activation.
struct HostToken(ActivationScope);

type Held = Arc<Mutex<Option<HostToken>>>;

struct Probe(Held);
impl PluginInstance for Probe {
    fn activate(&self, context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        let held = self.0.clone();
        Box::pin(async move {
            let token = context.capabilities.take::<HostToken>()?;
            *held.lock().unwrap() = Some(token);
            Ok(())
        })
    }
    fn command(
        &self,
        _: PluginCallContext,
        _: bmux_plugin_sdk::NativeCommandContext,
    ) -> PluginFuture<'_, CommandOutcome> {
        // Later commands reuse the activation's capability; no rebinding.
        let live = self
            .0
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|token| token.0.try_admit().is_ok());
        Box::pin(async move {
            CommandOutcome {
                exit_code: i32::from(!live),
                error: None,
                metadata: BTreeMap::new(),
            }
        })
    }
    fn deactivate(&self, _: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        self.0.lock().unwrap().take();
        Box::pin(async { Ok(()) })
    }
}

fn register(role: &str) -> (bmux_plugin::PluginRegistry, String, Held) {
    let plugin_id = format!("example.frontend-{}", uuid::Uuid::new_v4());
    let manifest = format!(
        "id='{plugin_id}'\nname='Frontend probe'\nversion='1.0.0'\ncomponent_roles=['{role}']\n"
    );
    let held: Held = Arc::default();
    let factory_held = held.clone();
    let roles = vec![if role == "frontend" {
        ComponentRole::Frontend
    } else {
        ComponentRole::Backend
    }];
    let definition = PluginDefinition::new(
        PluginMetadata {
            plugin_id: plugin_id.clone(),
            version: "1.0.0".into(),
            roles,
            required_capabilities: Vec::new(),
            contracts: Vec::new(),
            manifest_toml: Some(manifest),
        },
        move |_| Ok(Arc::new(Probe(factory_held.clone())) as Arc<dyn PluginInstance>),
    );
    let embedded = register_bundled_definition(&plugin_id, definition).unwrap();
    let mut registry = bmux_plugin::PluginRegistry::new();
    registry.register_bundled_manifest(&embedded).unwrap();
    (registry, plugin_id, held)
}

fn command_context(
    state: &RuntimeCommandState,
    plugin: &bmux_plugin::RegisteredPlugin,
) -> NativeCommandContext {
    plugin_command_context(
        &state.config,
        &state.paths,
        &plugin.declaration,
        "show",
        &[],
        Vec::new(),
        Vec::new(),
        vec![plugin.declaration.id.as_str().to_string()],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        None,
        bmux_plugin_sdk::NativeCommandInvocationSource::AttachKeybinding,
    )
}

fn with_runtime(test: impl FnOnce(crate::runtime_owner::RuntimeOwner, ConfigPaths)) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let driver = crate::runtime_owner::RuntimeDriver::new(runtime).unwrap();
    let owner = driver.owner();
    let root = tempfile::tempdir().unwrap();
    let paths = ConfigPaths::new(
        root.path().join("config"),
        root.path().join("runtime"),
        root.path().join("data"),
        root.path().join("state"),
    );
    test(owner.clone(), paths);
    drop(owner);
    driver.shutdown().unwrap();
}

#[test]
fn owner_binds_actual_activation_once_and_releases_it_on_deactivation() {
    with_runtime(|owner, paths| {
        let (registry, plugin_id, held) = register("frontend");
        let state = build_runtime_command_state(
            BmuxConfig::default(),
            paths,
            Arc::new(registry),
            Some(&owner),
        )
        .unwrap();
        let bound: Arc<Mutex<Vec<ActivationScope>>> = Arc::default();
        let retained = bound.clone();
        let frontends = Arc::new(OwnedFrontends::new(Arc::new(
            move |scope: &ActivationScope, capabilities: &ActivationCapabilities| {
                retained.lock().unwrap().push(scope.clone());
                capabilities
                    .insert(HostToken(scope.clone()))
                    .map_err(|error| bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: error.message,
                    })
            },
        )));
        let worker_frontends = frontends.clone();
        let worker_state = state.clone();
        let worker_id = plugin_id.clone();
        owner.handle().block_on(async move {
            tokio::task::spawn_blocking(move || {
                let _selected = SelectedCommandStateGuard::enter(&worker_state);
                let plugin = worker_state.registry.get(&worker_id).unwrap();
                let context = command_context(&worker_state, plugin);
                for _ in 0..2 {
                    let instance = worker_frontends
                        .instance(plugin, &worker_state, &context)
                        .unwrap()
                        .expect("frontend-only plugin is host-owned");
                    let (status, _) = instance
                        .run_command_with_context_and_outcome("show", &[], Some(&context))
                        .unwrap();
                    assert_eq!(status, 0, "command sees the activation's capability");
                }
            })
            .await
            .unwrap();
        });
        assert!(frontends.contains(&plugin_id));
        let scopes = bound.lock().unwrap().clone();
        assert_eq!(scopes.len(), 1, "one activation for the owner's lifetime");
        // The binder received the loader's real activation, which the
        // component also holds.
        let held_id = held.lock().unwrap().as_ref().map(|token| token.0.id());
        assert_eq!(held_id, Some(scopes[0].id()));
        assert!(scopes[0].try_admit().is_ok());

        owner
            .handle()
            .block_on(frontends.deactivate_all(&state))
            .unwrap();
        assert!(!frontends.contains(&plugin_id));
        assert!(held.lock().unwrap().is_none(), "component released it");
        assert!(scopes[0].try_admit().is_err(), "host half revoked");
    });
}

#[test]
fn missing_host_binding_fails_explicitly_and_backends_are_not_owned() {
    with_runtime(|owner, paths| {
        let (registry, plugin_id, held) = register("frontend");
        let (backend_registry, backend_id, _) = register("backend");
        let state = build_runtime_command_state(
            BmuxConfig::default(),
            paths.clone(),
            Arc::new(registry),
            Some(&owner),
        )
        .unwrap();
        let backend_state = build_runtime_command_state(
            BmuxConfig::default(),
            paths,
            Arc::new(backend_registry),
            Some(&owner),
        )
        .unwrap();
        owner.handle().block_on(async move {
            tokio::task::spawn_blocking(move || {
                // A command-local load has no host binder: activation fails
                // explicitly instead of reaching any global channel.
                let selected = SelectedCommandStateGuard::enter(&state);
                let plugin = state.registry.get(&plugin_id).unwrap();
                let context = command_context(&state, plugin);
                let loaded = load_plugin(
                    plugin,
                    &plugin_host_metadata(),
                    &BTreeMap::new(),
                    &state.config,
                )
                .unwrap();
                let error = loaded
                    .run_command_with_context_and_outcome("show", &[], Some(&context))
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("not supplied by host"), "{error}");
                assert!(held.lock().unwrap().is_none());
                // The failed activation still owes explicit cleanup.
                let lifecycle = crate::runtime::plugin_runtime::command_lifecycle_context(
                    &state,
                    &plugin.declaration,
                );
                tokio::runtime::Handle::current()
                    .block_on(loaded.deactivate_async(&lifecycle))
                    .unwrap();

                // A host whose binder cannot supply the ports refuses the
                // frontend explicitly and caches nothing.
                let refusing = OwnedFrontends::new(Arc::new(
                    |_: &ActivationScope, _: &ActivationCapabilities| {
                        Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                            details: "attachment has no frontend surface".into(),
                        })
                    },
                ));
                let error = format!(
                    "{:#}",
                    refusing.instance(plugin, &state, &context).err().unwrap()
                );
                assert!(error.contains("no frontend surface"), "{error}");
                assert!(!refusing.contains(&plugin_id));
                drop(selected);

                let frontends = OwnedFrontends::new(Arc::new(
                    |_: &ActivationScope, _: &ActivationCapabilities| Ok(()),
                ));
                let _selected = SelectedCommandStateGuard::enter(&backend_state);
                let backend = backend_state.registry.get(&backend_id).unwrap();
                let context = command_context(&backend_state, backend);
                assert!(
                    frontends
                        .instance(backend, &backend_state, &context)
                        .unwrap()
                        .is_none()
                );
            })
            .await
            .unwrap();
        });
    });
}

const PROBE_STATE: bmux_plugin_sdk::PluginEventKind =
    bmux_plugin_sdk::PluginEventKind::from_static("example.frontend/probe-state");

/// Publishes into the bus its activation was handed and holds its registry.
struct HandleProbe(Arc<Mutex<Option<Arc<bmux_plugin::PluginStateRegistry>>>>);
impl PluginInstance for HandleProbe {
    fn activate(&self, context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        let held = self.0.clone();
        Box::pin(async move {
            let bus = bmux_plugin::take_activation_event_bus(&context.capabilities)?;
            bus.publish_state(&PROBE_STATE, 7_u32).map_err(|error| {
                bmux_plugin_sdk::PluginCommandError::unavailable(error.to_string())
            })?;
            let states = bmux_plugin::take_activation_state_registry(&context.capabilities)?;
            *held.lock().unwrap() = Some(states);
            Ok(())
        })
    }
    fn command(
        &self,
        _: PluginCallContext,
        _: bmux_plugin_sdk::NativeCommandContext,
    ) -> PluginFuture<'_, CommandOutcome> {
        Box::pin(async { CommandOutcome::from(PluginResult::Ok(0)) })
    }
}

#[test]
fn owner_offers_its_event_bus_and_state_registry_to_frontends() {
    with_runtime(|owner, paths| {
        let plugin_id = format!("example.frontend-{}", uuid::Uuid::new_v4());
        let manifest = format!(
            "id='{plugin_id}'\nname='Frontend probe'\nversion='1.0.0'\ncomponent_roles=['frontend']\n"
        );
        let held: Arc<Mutex<Option<Arc<bmux_plugin::PluginStateRegistry>>>> = Arc::default();
        let factory_held = held.clone();
        let definition = PluginDefinition::new(
            PluginMetadata {
                plugin_id: plugin_id.clone(),
                version: "1.0.0".into(),
                roles: vec![ComponentRole::Frontend],
                required_capabilities: Vec::new(),
                contracts: Vec::new(),
                manifest_toml: Some(manifest),
            },
            move |_| Ok(Arc::new(HandleProbe(factory_held.clone())) as Arc<dyn PluginInstance>),
        );
        let embedded = register_bundled_definition(&plugin_id, definition).unwrap();
        let mut registry = bmux_plugin::PluginRegistry::new();
        registry.register_bundled_manifest(&embedded).unwrap();
        let state = build_runtime_command_state(
            BmuxConfig::default(),
            paths,
            Arc::new(registry),
            Some(&owner),
        )
        .unwrap();

        let events = Arc::new(bmux_plugin::EventBus::new());
        let _channel = events.register_state_channel::<u32>(PROBE_STATE, 0);
        let states = Arc::new(bmux_plugin::PluginStateRegistry::new());
        let frontends = Arc::new(
            OwnedFrontends::new(Arc::new(
                |_: &ActivationScope, _: &ActivationCapabilities| Ok(()),
            ))
            .with_host_handles(events.clone(), states),
        );
        let worker_frontends = frontends.clone();
        let worker_state = state.clone();
        owner.handle().block_on(async move {
            tokio::task::spawn_blocking(move || {
                let _selected = SelectedCommandStateGuard::enter(&worker_state);
                let plugin = worker_state.registry.get(&plugin_id).unwrap();
                let context = command_context(&worker_state, plugin);
                worker_frontends
                    .instance(plugin, &worker_state, &context)
                    .unwrap()
                    .expect("frontend-only plugin is host-owned");
            })
            .await
            .unwrap();
        });
        let (published, _) = events.subscribe_state::<u32>(&PROBE_STATE).unwrap();
        assert_eq!(*published, 7, "frontend published on the owner's bus");
        assert!(
            held.lock().unwrap().is_some(),
            "frontend received a state registry"
        );

        owner
            .handle()
            .block_on(frontends.deactivate_all(&state))
            .unwrap();
    });
}
