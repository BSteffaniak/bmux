use super::*;

pub(super) fn plugin_host_metadata() -> HostMetadata {
    HostMetadata {
        product_name: "bmux".to_string(),
        product_version: env!("CARGO_PKG_VERSION").to_string(),
        plugin_api_version: CURRENT_PLUGIN_API_VERSION,
        plugin_abi_version: CURRENT_PLUGIN_ABI_VERSION,
    }
}

pub(super) fn plugin_host_for_declaration(
    declaration: &bmux_plugin::PluginDeclaration,
    paths: &ConfigPaths,
    config: &BmuxConfig,
    available_services: Vec<RegisteredService>,
) -> plugin_host::CliPluginHost {
    plugin_host::CliPluginHost::for_plugin(
        declaration.id.as_str(),
        plugin_host_metadata(),
        paths,
        config.clone(),
        declaration.required_capabilities.clone(),
        declaration.provided_capabilities.clone(),
        available_services,
    )
}

#[cfg(test)]
pub(super) fn validate_configured_plugins(config: &BmuxConfig, paths: &ConfigPaths) -> Result<()> {
    let registry = scan_available_plugins(config, paths)?;
    validate_enabled_plugins(config, &registry)
}

/// Declare statically-linked bundled plugins from a single table.
///
/// This generates both `register_static_bundled_plugins` (registers each
/// plugin's manifest into the [`PluginRegistry`]) and `static_bundled_vtable`
/// (returns the [`StaticPluginVtable`] for a given plugin id).
///
/// Each entry is only compiled when its Cargo feature flag is active, so the
/// binary only includes the plugin code the user opted into (all four by
/// default via the `bundled-plugins` feature).
///
/// To add a new bundled plugin, append one four-line entry here -- no need to
/// touch two separate functions.
macro_rules! declare_bundled_plugins {
    ($(
        feature = $feature:literal,
        id = $id:literal,
        manifest = $manifest:expr,
        plugin_type = $ty:ty;
    )*) => {
        #[allow(unused_variables)]
        fn register_static_bundled_plugins(registry: &mut PluginRegistry) {
            $(
                #[cfg(feature = $feature)]
                if let Err(e) = registry.register_bundled_manifest($manifest) {
                    tracing::warn!(concat!("failed to register bundled ", $id, " plugin: {}"), e);
                }
            )*
        }

        #[allow(unused_variables)]
        fn static_bundled_vtable(plugin_id: &str) -> Option<bmux_plugin_sdk::StaticPluginVtable> {
            $(
                #[cfg(feature = $feature)]
                if plugin_id == $id {
                    return Some(bmux_plugin_sdk::bundled_plugin_vtable!($ty, $manifest));
                }
            )*
            None
        }
    };
}

declare_bundled_plugins! {
    feature = "bundled-plugin-clipboard",
    id = "bmux.clipboard",
    manifest = include_str!("../../../../plugins/clipboard-plugin/plugin.toml"),
    plugin_type = bmux_clipboard_plugin::ClipboardPlugin;

    feature = "bundled-plugin-permissions",
    id = "bmux.permissions",
    manifest = include_str!("../../../../plugins/permissions-plugin/plugin.toml"),
    plugin_type = bmux_permissions_plugin::PermissionsPlugin;

    feature = "bundled-plugin-cli",
    id = "bmux.plugin_cli",
    manifest = include_str!("../../../../plugins/plugin-cli-plugin/plugin.toml"),
    plugin_type = bmux_plugin_cli_plugin::PluginCliPlugin;

    feature = "bundled-plugin-windows",
    id = "bmux.windows",
    manifest = include_str!("../../../../plugins/windows-plugin/plugin.toml"),
    plugin_type = bmux_windows_plugin::WindowsPlugin;
}

/// Load a registered plugin, using the static vtable path for bundled plugins
/// and the dynamic `dlopen` path for everything else.
pub(super) fn load_plugin(
    plugin: &bmux_plugin::RegisteredPlugin,
    host: &HostMetadata,
    available_capabilities: &std::collections::BTreeMap<HostScope, bmux_plugin::CapabilityProvider>,
) -> bmux_plugin_sdk::Result<bmux_plugin::LoadedPlugin> {
    if plugin.bundled_static {
        let vtable = static_bundled_vtable(plugin.declaration.id.as_str()).ok_or_else(|| {
            bmux_plugin_sdk::PluginError::MissingStaticVtable {
                plugin_id: plugin.declaration.id.as_str().to_string(),
            }
        })?;
        bmux_plugin::load_static_plugin(plugin, vtable, host, available_capabilities)
    } else {
        load_native_registered_plugin(plugin, host, available_capabilities)
    }
}

pub(crate) fn scan_available_plugins(
    config: &BmuxConfig,
    paths: &ConfigPaths,
) -> Result<PluginRegistry> {
    let workspace_bundled_root = workspace_bundled_plugin_root();
    let search_paths = resolve_plugin_search_paths(config, paths)?;
    let reports = bmux_plugin::discover_plugin_manifests_in_roots(&search_paths)?;
    let mut registry = PluginRegistry::new();

    // Register statically-linked bundled plugins first (behind feature flags).
    register_static_bundled_plugins(&mut registry);

    for report in reports {
        for manifest_path in report.manifest_paths {
            match PluginManifest::from_path(&manifest_path) {
                Ok(mut manifest) => {
                    if let Some(entry_path) = manifest.resolve_entry_path(
                        manifest_path
                            .parent()
                            .unwrap_or_else(|| std::path::Path::new(".")),
                    ) {
                        if !entry_path.exists()
                            && workspace_bundled_root
                                .as_ref()
                                .is_some_and(|root| report.search_root == *root)
                            && let Ok(executable) = std::env::current_exe()
                            && let Some(executable_dir) = executable.parent()
                        {
                            if let Some(entry) = manifest.entry.as_ref() {
                                let executable_candidate = executable_dir.join(entry);
                                if executable_candidate.exists() {
                                    manifest.entry = Some(executable_candidate);
                                }
                            }
                        }
                    }
                    if let Err(error) = registry.register_manifest_from_root(
                        &report.search_root,
                        &manifest_path,
                        manifest,
                    ) {
                        // DuplicatePluginId is expected when a static-bundled
                        // plugin is also discovered on the filesystem -- skip
                        // since the static registration already won.
                        if matches!(
                            error,
                            bmux_plugin_sdk::PluginError::DuplicatePluginId { .. }
                        ) {
                            debug!(
                                "skipping filesystem plugin {} (duplicate of static-bundled plugin)",
                                manifest_path.display()
                            );
                        } else {
                            warn!(
                                "skipping plugin manifest {} during enabled-plugin scan: {error}",
                                manifest_path.display()
                            );
                        }
                    }
                }
                Err(error) => {
                    warn!(
                        "skipping unreadable plugin manifest {} during enabled-plugin scan: {error}",
                        manifest_path.display()
                    );
                }
            }
        }
    }
    Ok(registry)
}

pub(super) fn resolve_plugin_search_paths(
    config: &BmuxConfig,
    paths: &ConfigPaths,
) -> Result<Vec<PathBuf>> {
    let mut resolved = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for bundled in bundled_plugin_roots() {
        if seen.insert(bundled.clone()) {
            resolved.push(bundled);
        }
    }

    let user_plugins = paths.plugins_dir();
    if seen.insert(user_plugins.clone()) {
        resolved.push(user_plugins);
    }

    for search_path in &config.plugins.search_paths {
        let absolute = if search_path.is_absolute() {
            search_path.clone()
        } else {
            std::env::current_dir()
                .context("failed resolving current directory for plugin search path")?
                .join(search_path)
        };
        if seen.insert(absolute.clone()) {
            resolved.push(absolute);
        }
    }

    Ok(resolved)
}

pub(super) fn bundled_plugin_root() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let parent = executable.parent()?;
    Some(parent.join("plugins"))
}

pub(super) fn workspace_bundled_plugin_root() -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir.parent()?.parent()?;
    let plugins = root.join("plugins");
    plugins.exists().then_some(plugins)
}

pub(crate) fn bundled_plugin_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    if let Some(root) = bundled_plugin_root()
        && seen.insert(root.clone())
    {
        roots.push(root);
    }
    if let Some(root) = workspace_bundled_plugin_root()
        && seen.insert(root.clone())
    {
        roots.push(root);
    }
    roots
}

pub(crate) fn registered_plugin_entry_exists(plugin: &bmux_plugin::RegisteredPlugin) -> bool {
    plugin
        .manifest
        .resolve_entry_path(
            plugin
                .manifest_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new(".")),
        )
        .is_some_and(|path| path.exists())
}

/// Discover bundled plugin IDs using the same dynamic discovery as the runtime.
/// Returns an empty vec on failure (non-fatal).
pub(crate) fn discover_bundled_plugin_ids() -> Vec<String> {
    let config = BmuxConfig::default();
    let paths = bmux_config::ConfigPaths::default();
    let roots = bundled_plugin_roots();

    match scan_available_plugins(&config, &paths) {
        Ok(registry) => registry
            .iter()
            .filter(|plugin| {
                roots.contains(&plugin.search_root) && registered_plugin_entry_exists(plugin)
            })
            .map(|plugin| plugin.declaration.id.as_str().to_string())
            .collect(),
        Err(e) => {
            tracing::warn!("failed to discover bundled plugins, using empty list: {e:#}");
            Vec::new()
        }
    }
}

pub(super) fn effective_enabled_plugins(
    config: &BmuxConfig,
    registry: &PluginRegistry,
) -> Vec<String> {
    let disabled = config
        .plugins
        .disabled
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let bundled_roots = bundled_plugin_roots()
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let mut enabled = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    // Auto-enable statically-linked bundled plugins (always available, no
    // entry file to check).
    let mut static_bundled = registry
        .iter()
        .filter(|&plugin| plugin.bundled_static)
        .map(|plugin| plugin.declaration.id.as_str().to_string())
        .collect::<Vec<_>>();
    static_bundled.sort();
    for plugin_id in static_bundled {
        if disabled.contains(plugin_id.as_str()) {
            continue;
        }
        if seen.insert(plugin_id.clone()) {
            enabled.push(plugin_id);
        }
    }

    // Auto-enable filesystem-discovered bundled plugins (from bundled roots
    // whose entry file exists on disk).
    let mut bundled_defaults = registry
        .iter()
        .filter(|&plugin| {
            !plugin.bundled_static
                && bundled_roots.contains(&plugin.search_root)
                && registered_plugin_entry_exists(plugin)
        })
        .map(|plugin| plugin.declaration.id.as_str().to_string())
        .collect::<Vec<_>>();
    bundled_defaults.sort();
    for plugin_id in bundled_defaults {
        if disabled.contains(plugin_id.as_str()) {
            continue;
        }
        if seen.insert(plugin_id.clone()) {
            enabled.push(plugin_id);
        }
    }

    for plugin_id in &config.plugins.enabled {
        if disabled.contains(plugin_id.as_str()) {
            continue;
        }
        if seen.insert(plugin_id.clone()) {
            enabled.push(plugin_id.clone());
        }
    }

    enabled
}

pub(super) fn validate_enabled_plugins(
    config: &BmuxConfig,
    registry: &PluginRegistry,
) -> Result<()> {
    let disabled = config
        .plugins
        .disabled
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let enabled_plugins = effective_enabled_plugins(config, registry);
    if enabled_plugins.is_empty() {
        return Ok(());
    }

    for plugin_id in &config.plugins.enabled {
        if disabled.contains(plugin_id.as_str()) {
            continue;
        }
        let _ = registry.get(plugin_id).with_context(|| {
            let available = registry.plugin_ids();
            if available.is_empty() {
                format!(
                    "enabled plugin '{plugin_id}' was not found in the configured plugins directory"
                )
            } else {
                format!(
                    "enabled plugin '{plugin_id}' was not found in the configured plugins directory (available: {})",
                    available.join(", ")
                )
            }
        })?;
    }

    let _ = registry
        .activation_order_for(&enabled_plugins)
        .context("enabled plugin dependency graph is invalid")?;

    let mut command_config = config.clone();
    command_config.plugins.enabled = enabled_plugins;
    PluginCommandRegistry::build(&command_config, registry)
        .context("failed building plugin CLI command registry")?;

    Ok(())
}

pub(super) fn load_enabled_plugins(
    config: &BmuxConfig,
    registry: &PluginRegistry,
) -> Result<Vec<bmux_plugin::LoadedPlugin>> {
    let enabled_plugins = effective_enabled_plugins(config, registry);
    if enabled_plugins.is_empty() {
        return Ok(Vec::new());
    }

    let disabled = config
        .plugins
        .disabled
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let explicitly_enabled = config
        .plugins
        .enabled
        .iter()
        .filter(|plugin_id| !disabled.contains(plugin_id.as_str()))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();

    for plugin_id in &enabled_plugins {
        if registry.get(plugin_id).is_some() {
            continue;
        }
        if explicitly_enabled.contains(plugin_id) {
            anyhow::bail!("enabled plugin '{plugin_id}' disappeared during native load");
        }
        warn!("skipping bundled plugin '{plugin_id}' because it is no longer discoverable");
    }

    let host = plugin_host_metadata();
    let available_capabilities = available_capability_providers(config, registry)?;
    let ordered_plugins = registry
        .activation_order_for(&enabled_plugins)
        .context("enabled plugin dependency graph is invalid")?;
    let mut loaded_plugins = Vec::with_capacity(ordered_plugins.len());
    for plugin in ordered_plugins {
        let plugin_id = plugin.declaration.id.as_str();
        let loaded = match load_plugin(plugin, &host, &available_capabilities) {
            Ok(loaded) => loaded,
            Err(error) => {
                if explicitly_enabled.contains(plugin_id) {
                    return Err(error)
                        .with_context(|| format!("failed loading enabled plugin '{plugin_id}'"));
                }
                warn!("skipping bundled plugin '{plugin_id}': {error}");
                continue;
            }
        };
        loaded_plugins.push(loaded);
    }

    Ok(loaded_plugins)
}

pub(super) fn registered_plugin_infos_from_loaded(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
) -> Vec<bmux_plugin_sdk::RegisteredPluginInfo> {
    loaded_plugins
        .iter()
        .map(|plugin| bmux_plugin_sdk::RegisteredPluginInfo {
            id: plugin.declaration.id.as_str().to_string(),
            display_name: plugin.declaration.display_name.clone(),
            version: plugin.declaration.plugin_version.clone(),
            bundled_static: plugin.registered.bundled_static,
            required_capabilities: plugin
                .declaration
                .required_capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            provided_capabilities: plugin
                .declaration
                .provided_capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            commands: plugin
                .declaration
                .commands
                .iter()
                .map(|c| c.name.clone())
                .collect(),
        })
        .collect()
}

pub(super) fn registered_plugin_infos_from_registry(
    registry: &PluginRegistry,
) -> Vec<bmux_plugin_sdk::RegisteredPluginInfo> {
    registry
        .iter()
        .map(|plugin| bmux_plugin_sdk::RegisteredPluginInfo {
            id: plugin.declaration.id.as_str().to_string(),
            display_name: plugin.declaration.display_name.clone(),
            version: plugin.declaration.plugin_version.clone(),
            bundled_static: plugin.bundled_static,
            required_capabilities: plugin
                .declaration
                .required_capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            provided_capabilities: plugin
                .declaration
                .provided_capabilities
                .iter()
                .map(ToString::to_string)
                .collect(),
            commands: plugin
                .declaration
                .commands
                .iter()
                .map(|c| c.name.clone())
                .collect(),
        })
        .collect()
}

pub(super) fn plugin_lifecycle_context(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    declaration: &bmux_plugin::PluginDeclaration,
    available_services: Vec<RegisteredService>,
    available_capabilities: Vec<String>,
    enabled_plugins: Vec<String>,
    plugin_search_roots: Vec<String>,
    registered_plugins: Vec<bmux_plugin_sdk::RegisteredPluginInfo>,
) -> NativeLifecycleContext {
    let host = plugin_host_for_declaration(declaration, paths, config, available_services.clone());
    NativeLifecycleContext {
        plugin_id: declaration.id.as_str().to_string(),
        required_capabilities: declaration
            .required_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        provided_capabilities: declaration
            .provided_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        services: available_services,
        available_capabilities,
        enabled_plugins,
        plugin_search_roots,
        registered_plugins,
        host: plugin_host_metadata(),
        connection: bmux_plugin_sdk::PluginHost::connection(&host).clone(),
        settings: config
            .plugins
            .settings
            .get(declaration.id.as_str())
            .cloned(),
        plugin_settings_map: config.plugins.settings.clone(),
        host_kernel_bridge: Some(bmux_plugin_sdk::HostKernelBridge::from_fn(
            host_kernel_bridge,
        )),
    }
}

pub(super) fn plugin_command_context(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    declaration: &bmux_plugin::PluginDeclaration,
    command: &str,
    arguments: &[String],
    available_services: Vec<RegisteredService>,
    available_capabilities: Vec<String>,
    enabled_plugins: Vec<String>,
    plugin_search_roots: Vec<String>,
    registered_plugins: Vec<bmux_plugin_sdk::RegisteredPluginInfo>,
) -> NativeCommandContext {
    let host = plugin_host_for_declaration(declaration, paths, config, available_services.clone());
    NativeCommandContext {
        plugin_id: declaration.id.as_str().to_string(),
        command: command.to_string(),
        arguments: arguments.to_vec(),
        required_capabilities: declaration
            .required_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        provided_capabilities: declaration
            .provided_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        services: available_services,
        available_capabilities,
        enabled_plugins,
        plugin_search_roots,
        registered_plugins,
        host: plugin_host_metadata(),
        connection: bmux_plugin_sdk::PluginHost::connection(&host).clone(),
        settings: config
            .plugins
            .settings
            .get(declaration.id.as_str())
            .cloned(),
        plugin_settings_map: config.plugins.settings.clone(),
        host_kernel_bridge: Some(bmux_plugin_sdk::HostKernelBridge::from_fn(
            host_kernel_bridge,
        )),
    }
}

pub(super) fn plugin_system_event(name: &str) -> PluginEvent {
    PluginEvent {
        kind: PluginEventKind::System,
        name: name.to_string(),
        payload: serde_json::json!({
            "product": "bmux",
            "version": env!("CARGO_PKG_VERSION"),
        }),
    }
}

pub(super) fn activate_loaded_plugins(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
    config: &BmuxConfig,
    paths: &ConfigPaths,
) -> Result<()> {
    let mut activated: Vec<&bmux_plugin::LoadedPlugin> = Vec::new();
    let connection_info = HostConnectionInfo {
        config_dir: paths.config_dir.to_string_lossy().into_owned(),
        runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
        data_dir: paths.data_dir.to_string_lossy().into_owned(),
        state_dir: paths.state_dir.to_string_lossy().into_owned(),
    };
    let plugin_search_roots = resolve_plugin_search_paths(config, paths)?
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let available_capabilities = core_provided_capabilities()
        .into_iter()
        .chain(
            loaded_plugins
                .iter()
                .flat_map(|plugin| plugin.declaration.provided_capabilities.iter().cloned()),
        )
        .map(|capability| capability.to_string())
        .collect::<Vec<_>>();
    let available_services = service_descriptors_from_declarations(
        loaded_plugins.iter().map(|plugin| &plugin.declaration),
    );
    let enabled_plugins = loaded_plugins
        .iter()
        .map(|plugin| plugin.declaration.id.as_str().to_string())
        .collect::<Vec<_>>();
    let registered_plugins = registered_plugin_infos_from_loaded(loaded_plugins);
    for plugin in loaded_plugins {
        if !plugin.declaration.lifecycle.activate_on_startup {
            continue;
        }

        let context = plugin_lifecycle_context(
            config,
            paths,
            &plugin.declaration,
            available_services.clone(),
            available_capabilities.clone(),
            enabled_plugins.clone(),
            plugin_search_roots.clone(),
            registered_plugins.clone(),
        );
        let _host_kernel_connection_guard = enter_host_kernel_connection(connection_info.clone());
        if let Err(error) = plugin.activate(&context) {
            for activated_plugin in activated.into_iter().rev() {
                let context = plugin_lifecycle_context(
                    config,
                    paths,
                    &activated_plugin.declaration,
                    available_services.clone(),
                    available_capabilities.clone(),
                    enabled_plugins.clone(),
                    plugin_search_roots.clone(),
                    registered_plugins.clone(),
                );
                let _host_kernel_connection_guard =
                    enter_host_kernel_connection(connection_info.clone());
                if let Err(deactivate_error) = activated_plugin.deactivate(&context) {
                    warn!(
                        "failed rolling back plugin activation for {}: {deactivate_error}",
                        activated_plugin.declaration.id.as_str()
                    );
                }
            }
            return Err(error).with_context(|| {
                format!(
                    "failed activating plugin '{}'",
                    plugin.declaration.id.as_str()
                )
            });
        }

        activated.push(plugin);
    }

    Ok(())
}

pub(super) fn deactivate_loaded_plugins(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
    config: &BmuxConfig,
    paths: &ConfigPaths,
) -> Result<()> {
    let connection_info = HostConnectionInfo {
        config_dir: paths.config_dir.to_string_lossy().into_owned(),
        runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
        data_dir: paths.data_dir.to_string_lossy().into_owned(),
        state_dir: paths.state_dir.to_string_lossy().into_owned(),
    };
    let plugin_search_roots = resolve_plugin_search_paths(config, paths)?
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let available_capabilities = core_provided_capabilities()
        .into_iter()
        .chain(
            loaded_plugins
                .iter()
                .flat_map(|plugin| plugin.declaration.provided_capabilities.iter().cloned()),
        )
        .map(|capability| capability.to_string())
        .collect::<Vec<_>>();
    let available_services = service_descriptors_from_declarations(
        loaded_plugins.iter().map(|plugin| &plugin.declaration),
    );
    let enabled_plugins = loaded_plugins
        .iter()
        .map(|plugin| plugin.declaration.id.as_str().to_string())
        .collect::<Vec<_>>();
    let registered_plugins = registered_plugin_infos_from_loaded(loaded_plugins);
    for plugin in loaded_plugins.iter().rev() {
        if !plugin.declaration.lifecycle.activate_on_startup {
            continue;
        }

        let context = plugin_lifecycle_context(
            config,
            paths,
            &plugin.declaration,
            available_services.clone(),
            available_capabilities.clone(),
            enabled_plugins.clone(),
            plugin_search_roots.clone(),
            registered_plugins.clone(),
        );
        let _host_kernel_connection_guard = enter_host_kernel_connection(connection_info.clone());
        let _ = plugin.deactivate(&context).with_context(|| {
            format!(
                "failed deactivating plugin '{}'",
                plugin.declaration.id.as_str()
            )
        })?;
    }

    Ok(())
}

pub(super) fn dispatch_loaded_plugin_event(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
    event: PluginEvent,
) -> Result<()> {
    for plugin in loaded_plugins {
        let _ = plugin.dispatch_event(&event).with_context(|| {
            format!(
                "failed dispatching plugin event '{}' to '{}'",
                event.name,
                plugin.declaration.id.as_str()
            )
        })?;
    }

    Ok(())
}

pub(super) async fn plugin_event_bridge_loop(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    if loaded_plugins.is_empty() {
        return Ok(());
    }

    let mut client = loop {
        if *shutdown_rx.borrow() {
            return Ok(());
        }

        match connect_raw("bmux-plugin-event-bridge").await {
            Ok(client) => break client,
            Err(_) => {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
        }
    };

    client
        .subscribe_events()
        .await
        .map_err(map_cli_client_error)?;
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return Ok(());
                }
            }
            result = client.poll_events(32) => {
                let events = result.map_err(map_cli_client_error)?;
                for event in events {
                    dispatch_loaded_plugin_event(loaded_plugins, plugin_event_from_server_event(&event)?)?;
                }
            }
        }
    }
}

pub(super) fn plugin_event_from_server_event(
    event: &bmux_client::ServerEvent,
) -> Result<PluginEvent> {
    Ok(PluginEvent {
        kind: plugin_event_kind_from_server_event(event),
        name: server_event_name(event).to_string(),
        payload: serde_json::to_value(event).context("failed encoding server event payload")?,
    })
}

pub(super) const fn plugin_event_kind_from_server_event(
    event: &bmux_client::ServerEvent,
) -> PluginEventKind {
    match event {
        bmux_client::ServerEvent::ServerStarted | bmux_client::ServerEvent::ServerStopping => {
            PluginEventKind::System
        }
        bmux_client::ServerEvent::SessionCreated { .. }
        | bmux_client::ServerEvent::SessionRemoved { .. }
        | bmux_client::ServerEvent::FollowStarted { .. }
        | bmux_client::ServerEvent::FollowStopped { .. }
        | bmux_client::ServerEvent::FollowTargetGone { .. }
        | bmux_client::ServerEvent::FollowTargetChanged { .. } => PluginEventKind::Session,
        bmux_client::ServerEvent::ClientAttached { .. }
        | bmux_client::ServerEvent::ClientDetached { .. } => PluginEventKind::Client,
        bmux_client::ServerEvent::AttachViewChanged { .. } => PluginEventKind::Pane,
    }
}

pub(super) async fn run_plugin_command(
    plugin_id: &str,
    command_name: &str,
    args: &[String],
) -> Result<u8> {
    let status = run_plugin_command_internal(plugin_id, command_name, args)?.status;
    Ok(status.clamp(0, i32::from(u8::MAX)) as u8)
}

pub(super) fn run_plugin_keybinding_command(
    plugin_id: &str,
    command_name: &str,
    args: &[String],
) -> Result<PluginCommandExecution> {
    run_plugin_command_internal(plugin_id, command_name, args)
}

pub(super) struct PluginCommandExecution {
    pub(super) status: i32,
    pub(super) outcome: PluginCommandOutcome,
}

pub(super) fn run_plugin_command_internal(
    plugin_id: &str,
    command_name: &str,
    args: &[String],
) -> Result<PluginCommandExecution> {
    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    let available = registry.plugin_ids();
    let plugin = registry
        .get(plugin_id)
        .with_context(|| format_plugin_not_found_message(plugin_id, &available))?;
    let enabled_plugins = effective_enabled_plugins(&config, &registry);

    if !enabled_plugins.iter().any(|enabled| enabled == plugin_id) {
        anyhow::bail!(format_plugin_not_enabled_message(plugin_id));
    }

    let loaded = load_plugin(
        plugin,
        &plugin_host_metadata(),
        &available_capability_providers(&config, &registry)?,
    )
    .with_context(|| format!("failed loading enabled plugin '{plugin_id}'"))?;
    let plugin_search_roots = resolve_plugin_search_paths(&config, &paths)?
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let available_capabilities = available_capability_providers(&config, &registry)?
        .into_keys()
        .map(|capability| capability.to_string())
        .collect::<Vec<_>>();
    let context = plugin_command_context(
        &config,
        &paths,
        &plugin.declaration,
        command_name,
        args,
        available_service_descriptors(&config, &registry)?,
        available_capabilities,
        enabled_plugins,
        plugin_search_roots,
        registered_plugin_infos_from_registry(&registry),
    );
    begin_host_kernel_effect_capture();
    let _host_kernel_connection_guard = enter_host_kernel_connection(context.connection.clone());
    let run_result =
        loaded.run_command_with_context_and_outcome(command_name, args, Some(&context));
    let fallback_effects = finish_host_kernel_effect_capture();
    let (status, mut outcome) = run_result.map_err(|error| {
        anyhow::anyhow!(format_plugin_command_run_error(
            plugin_id,
            command_name,
            &error
        ))
    })?;
    if outcome.effects.is_empty() && !fallback_effects.is_empty() {
        let mut seen = std::collections::BTreeSet::new();
        for effect in fallback_effects {
            match effect {
                PluginCommandEffect::SelectContext { context_id } if seen.insert(context_id) => {
                    outcome
                        .effects
                        .push(PluginCommandEffect::SelectContext { context_id });
                }
                PluginCommandEffect::SelectContext { .. } => {}
            }
        }
    }
    Ok(PluginCommandExecution { status, outcome })
}

pub(super) fn format_plugin_command_run_error(
    plugin_id: &str,
    command_name: &str,
    error: &dyn std::fmt::Display,
) -> String {
    let base = format!("failed running plugin command '{plugin_id}:{command_name}': {error}");
    if base.contains("session policy denied for this operation") {
        format!(
            "{base}\nHint: operation denied by an active policy provider. Verify policy state or run with an authorized principal."
        )
    } else {
        base
    }
}

pub(super) fn format_plugin_not_found_message<S: AsRef<str>>(
    plugin_id: &str,
    available: &[S],
) -> String {
    if available.is_empty() {
        format!("plugin '{plugin_id}' was not found")
    } else {
        let available = available
            .iter()
            .map(std::convert::AsRef::as_ref)
            .collect::<Vec<_>>();
        format!(
            "plugin '{plugin_id}' was not found (available: {})",
            available.join(", ")
        )
    }
}

pub(super) fn format_plugin_not_enabled_message(plugin_id: &str) -> String {
    format!(
        "plugin '{plugin_id}' is not enabled; remove it from plugins.disabled or add it under plugins.enabled to run commands"
    )
}

pub(super) fn unknown_external_command_message(args: &[String]) -> String {
    format!(
        "unknown command '{}'; run 'bmux plugin list' to inspect available plugins",
        args.join(" ")
    )
}

pub(super) fn format_plugin_argument_validation_error(
    command_path: &[String],
    error: &dyn std::fmt::Display,
) -> String {
    let base = format!(
        "failed validating plugin command arguments for '{}': {error}",
        command_path.join(" ")
    );
    if base.contains("missing required") {
        format!("{base}\nHint: run '<command> --help' to inspect required plugin options.")
    } else {
        base
    }
}

pub(super) async fn run_external_plugin_command(args: &[String]) -> Result<u8> {
    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    let mut command_config = config.clone();
    command_config.plugins.enabled = effective_enabled_plugins(&config, &registry);
    let command_registry = PluginCommandRegistry::build(&command_config, &registry)
        .context("failed building plugin CLI command registry")?;
    let resolved = command_registry
        .resolve(args)
        .with_context(|| unknown_external_command_message(args))?;
    let validated_arguments =
        PluginCommandRegistry::validate_arguments(&resolved.schema, &resolved.arguments).map_err(
            |error| anyhow::anyhow!(format_plugin_argument_validation_error(args, &error)),
        )?;
    run_plugin_command(
        &resolved.plugin_id,
        &resolved.command_name,
        &validated_arguments,
    )
    .await
}
#[cfg(test)]
mod tests {
    fn temp_dir() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time should be monotonic for test")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bmux-cli-plugin-test-{nanos}"));
        std::fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    use crate::input::InputProcessor;
    use crate::runtime::attach::state::AttachViewState;
    use crate::runtime::*;
    use bmux_cli_schema::{Cli, Command};
    use bmux_client::{AttachLayoutState, AttachOpenInfo, ClientError};
    use bmux_config::{BmuxConfig, ConfigPaths, ResolvedTimeout};
    use bmux_ipc::transport::IpcTransportError;
    use bmux_ipc::{
        AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
        AttachViewComponent, ErrorCode, PaneLayoutNode, PaneSummary, RecordingSummary,
        SessionSummary,
    };
    use bmux_plugin::{PluginManifest, PluginRegistry};
    use bmux_plugin_sdk::PluginCommandEffect;
    use crossterm::event::{
        Event as CrosstermEvent, KeyCode as CrosstermKeyCode, KeyEvent as CrosstermKeyEvent,
        KeyEventKind as CrosstermKeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    fn plugin_manifest(id: &str, entry: &str) -> PluginManifest {
        plugin_manifest_with_commands(id, entry, "")
    }

    fn plugin_manifest_with_commands(id: &str, entry: &str, commands: &str) -> PluginManifest {
        let manifest = format!(
            "id = \"{id}\"\nname = \"{id}\"\nversion = \"0.1.0\"\nentry = \"{entry}\"\n{commands}\n"
        );
        PluginManifest::from_toml_str(&manifest).expect("manifest should parse")
    }

    #[test]
    fn validate_enabled_plugins_accepts_registered_plugin() {
        let dir = temp_dir();
        let plugin_dir = dir.join("example");
        fs::create_dir_all(&plugin_dir).expect("plugin dir should exist");
        fs::write(plugin_dir.join("example.dylib"), []).expect("entry should be written");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(
                &plugin_dir.join("plugin.toml"),
                plugin_manifest("example.plugin", "example.dylib"),
            )
            .expect("plugin should register");

        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("example.plugin".to_string());

        assert!(crate::runtime::validate_enabled_plugins(&config, &registry).is_ok());
    }

    #[test]
    fn effective_enabled_plugins_includes_bundled_plugins_by_default() {
        let Some(bundled_root) = crate::runtime::bundled_plugin_root() else {
            return;
        };
        let dir = temp_dir();
        fs::write(dir.join("windows.dylib"), []).expect("entry should be written");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest_from_root(
                &bundled_root,
                &dir.join("plugin.toml"),
                plugin_manifest("bmux.windows", "windows.dylib"),
            )
            .expect("bundled plugin should register");

        let config = BmuxConfig::default();
        let enabled = crate::runtime::effective_enabled_plugins(&config, &registry);
        assert!(enabled.iter().any(|plugin_id| plugin_id == "bmux.windows"));
    }

    #[test]
    fn effective_enabled_plugins_include_windows_and_permissions_by_default() {
        let Some(bundled_root) = crate::runtime::bundled_plugin_root() else {
            return;
        };
        let dir = temp_dir();
        fs::write(dir.join("windows.dylib"), []).expect("windows entry should be written");
        fs::write(dir.join("permissions.dylib"), []).expect("permissions entry should be written");

        let mut registry = PluginRegistry::new();
        registry
            .register_manifest_from_root(
                &bundled_root,
                &dir.join("windows.toml"),
                plugin_manifest("bmux.windows", "windows.dylib"),
            )
            .expect("windows plugin should register");
        registry
            .register_manifest_from_root(
                &bundled_root,
                &dir.join("permissions.toml"),
                plugin_manifest("bmux.permissions", "permissions.dylib"),
            )
            .expect("permissions plugin should register");

        let config = BmuxConfig::default();
        let enabled = crate::runtime::effective_enabled_plugins(&config, &registry);
        assert!(enabled.iter().any(|plugin_id| plugin_id == "bmux.windows"));
        assert!(
            enabled
                .iter()
                .any(|plugin_id| plugin_id == "bmux.permissions")
        );
    }

    #[test]
    fn effective_enabled_plugins_honors_disabled_overrides() {
        let Some(bundled_root) = crate::runtime::bundled_plugin_root() else {
            return;
        };
        let dir = temp_dir();
        fs::write(dir.join("windows.dylib"), []).expect("entry should be written");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest_from_root(
                &bundled_root,
                &dir.join("plugin.toml"),
                plugin_manifest("bmux.windows", "windows.dylib"),
            )
            .expect("bundled plugin should register");

        let mut config = BmuxConfig::default();
        config.plugins.disabled.push("bmux.windows".to_string());
        let enabled = crate::runtime::effective_enabled_plugins(&config, &registry);
        assert!(!enabled.iter().any(|plugin_id| plugin_id == "bmux.windows"));
    }

    #[test]
    fn effective_enabled_plugins_skips_bundled_plugins_with_missing_entry() {
        let Some(bundled_root) = crate::runtime::bundled_plugin_root() else {
            return;
        };
        let dir = temp_dir();
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest_from_root(
                &bundled_root,
                &dir.join("plugin.toml"),
                plugin_manifest("bmux.windows", "windows.dylib"),
            )
            .expect("bundled plugin should register");

        let config = BmuxConfig::default();
        let enabled = crate::runtime::effective_enabled_plugins(&config, &registry);
        assert!(!enabled.iter().any(|plugin_id| plugin_id == "bmux.windows"));
    }

    #[test]
    fn validate_enabled_plugins_accepts_plugin_provided_capabilities() {
        let dir = temp_dir();
        let provider_dir = dir.join("provider");
        let dependent_dir = dir.join("consumer");
        fs::create_dir_all(&provider_dir).expect("provider dir should exist");
        fs::create_dir_all(&dependent_dir).expect("dependent dir should exist");
        fs::write(provider_dir.join("provider.dylib"), []).expect("provider entry should exist");
        fs::write(dependent_dir.join("consumer.dylib"), []).expect("dependent entry should exist");

        let mut registry = PluginRegistry::new();
        registry
                .register_manifest(
                    &provider_dir.join("plugin.toml"),
                    PluginManifest::from_toml_str(
                        "id='provider.plugin'\nname='Provider'\nversion='0.1.0'\nentry='provider.dylib'\nrequired_capabilities=['bmux.commands']\nprovided_capabilities=['example.cap.read','example.cap.write']\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n",
                    )
                    .expect("provider manifest should parse"),
                )
                .expect("provider should register");
        registry
                .register_manifest(
                    &dependent_dir.join("plugin.toml"),
                    PluginManifest::from_toml_str(
                        "id='consumer.plugin'\nname='Consumer'\nversion='0.1.0'\nentry='consumer.dylib'\nrequired_capabilities=['example.cap.read']\n[[dependencies]]\nplugin_id='provider.plugin'\nversion_req='^0.1'\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n",
                    )
                    .expect("dependent manifest should parse"),
                )
                .expect("dependent should register");

        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("provider.plugin".to_string());
        config.plugins.enabled.push("consumer.plugin".to_string());

        assert!(crate::runtime::validate_enabled_plugins(&config, &registry).is_ok());
    }

    #[test]
    fn validate_enabled_plugins_rejects_missing_plugin() {
        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("missing.plugin".to_string());

        let error = crate::runtime::validate_enabled_plugins(&config, &PluginRegistry::new())
            .expect_err("validation should fail");
        assert!(error.to_string().contains("missing.plugin"));
    }

    #[test]
    fn validate_configured_plugins_discovers_plugins_from_default_layout() {
        let dir = temp_dir();
        let plugin_dir = dir.join("data").join("plugins").join("example");
        fs::create_dir_all(&plugin_dir).expect("plugin dir should exist");
        fs::write(plugin_dir.join("example.dylib"), []).expect("entry should be written");
        fs::write(
                plugin_dir.join("plugin.toml"),
                "id = 'example.plugin'\nname = 'Example'\nversion='0.1.0'\nentry='example.dylib'\nrequired_capabilities=['bmux.commands']\n[plugin_api]\nminimum='1.0'\n[native_abi]\nminimum='1.0'\n",
            )
            .expect("manifest should be written");

        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("example.plugin".to_string());
        let paths = ConfigPaths::new(
            dir.join("config"),
            dir.join("runtime"),
            dir.join("data"),
            dir.join("state"),
        );

        assert!(crate::runtime::validate_configured_plugins(&config, &paths).is_ok());
    }

    #[test]
    fn runtime_cli_prefers_dynamic_session_plugin_aliases_over_static_cli_rejection() {
        let dir = temp_dir();
        let plugin_dir = dir.join("policy");
        fs::create_dir_all(&plugin_dir).expect("plugin dir should exist");
        fs::write(plugin_dir.join("policy.dylib"), []).expect("entry should be written");

        let mut registry = PluginRegistry::new();
        registry
                .register_manifest(
                    &plugin_dir.join("plugin.toml"),
                    plugin_manifest_with_commands(
                        "policy.plugin",
                        "policy.dylib",
                        "[[commands]]\nname='roles'\npath=['roles']\naliases=[[\"session\",\"roles\"]]\nsummary='list'\nexecution='provider_exec'\nexpose_in_cli=true\n[[commands.arguments]]\nname='session'\nkind='string'\nlong='session'\nrequired=true\n",
                    ),
                )
                .expect("plugin should register");

        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("policy.plugin".to_string());
        let argv = vec![
            OsString::from("bmux"),
            OsString::from("session"),
            OsString::from("roles"),
            OsString::from("--session"),
            OsString::from("dev"),
        ];

        let parsed = crate::runtime::parse_runtime_cli_with_registry(&argv, &config, &registry)
            .expect("runtime CLI should parse plugin alias under session namespace");
        match parsed {
            crate::runtime::ParsedRuntimeCli::Plugin {
                plugin_id,
                command_name,
                arguments,
                ..
            } => {
                assert_eq!(plugin_id, "policy.plugin");
                assert_eq!(command_name, "roles");
                assert_eq!(arguments, vec!["--session".to_string(), "dev".to_string()]);
            }
            other => panic!("expected plugin runtime parse, got {other:?}"),
        }
    }

    #[test]
    fn runtime_cli_allows_plugin_owned_plugin_namespace_commands() {
        let dir = temp_dir();
        let plugin_dir = dir.join("plugin-cli");
        fs::create_dir_all(&plugin_dir).expect("plugin dir should exist");
        fs::write(plugin_dir.join("plugin-cli.dylib"), []).expect("entry should be written");

        let mut registry = PluginRegistry::new();
        registry
                .register_manifest(
                    &plugin_dir.join("plugin.toml"),
                    plugin_manifest_with_commands(
                        "bmux.plugin_cli",
                        "plugin-cli.dylib",
                        "[[commands]]\nname='list'\npath=['plugin','list']\nsummary='list'\nexecution='provider_exec'\nexpose_in_cli=true\n",
                    ),
                )
                .expect("plugin should register");

        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("bmux.plugin_cli".to_string());
        let argv = vec![
            OsString::from("bmux"),
            OsString::from("plugin"),
            OsString::from("list"),
        ];

        let parsed = crate::runtime::parse_runtime_cli_with_registry(&argv, &config, &registry)
            .expect("runtime CLI should parse plugin-owned plugin namespace command");
        match parsed {
            crate::runtime::ParsedRuntimeCli::Plugin {
                plugin_id,
                command_name,
                arguments,
                ..
            } => {
                assert_eq!(plugin_id, "bmux.plugin_cli");
                assert_eq!(command_name, "list");
                assert!(arguments.is_empty());
            }
            other => panic!("expected plugin runtime parse, got {other:?}"),
        }
    }

    #[test]
    fn runtime_cli_parses_bundled_plugin_command_without_explicit_enable() {
        let Some(bundled_root) = crate::runtime::bundled_plugin_root() else {
            return;
        };
        let dir = temp_dir();
        fs::write(dir.join("windows.dylib"), []).expect("entry should be written");
        let mut registry = PluginRegistry::new();
        registry
                .register_manifest_from_root(
                    &bundled_root,
                    &dir.join("plugin.toml"),
                    plugin_manifest_with_commands(
                        "bmux.windows",
                        "windows.dylib",
                        "[[commands]]\nname='new-window'\npath=['new-window']\nsummary='new'\nexecution='provider_exec'\nexpose_in_cli=true\n",
                    ),
                )
                .expect("plugin should register");

        let config = BmuxConfig::default();
        let argv = vec![OsString::from("bmux"), OsString::from("new-window")];
        let parsed = crate::runtime::parse_runtime_cli_with_registry(&argv, &config, &registry)
            .expect("runtime CLI should parse bundled plugin command");
        match parsed {
            crate::runtime::ParsedRuntimeCli::Plugin { plugin_id, .. } => {
                assert_eq!(plugin_id, "bmux.windows");
            }
            other => panic!("expected plugin runtime parse, got {other:?}"),
        }
    }

    #[test]
    fn runtime_cli_attach_remains_builtin_without_windows_plugin() {
        let config = BmuxConfig::default();
        let registry = PluginRegistry::new();
        let argv = vec![
            OsString::from("bmux"),
            OsString::from("attach"),
            OsString::from("dev"),
        ];

        let parsed = crate::runtime::parse_runtime_cli_with_registry(&argv, &config, &registry)
            .expect("runtime CLI should parse built-in attach command");

        match parsed {
            crate::runtime::ParsedRuntimeCli::BuiltIn { cli, .. } => {
                assert!(matches!(
                    cli.command,
                    Some(Command::Attach {
                        target: Some(ref target),
                        follow: None,
                        global: false,
                    }) if target == "dev"
                ));
            }
            other => panic!("expected built-in CLI parse, got {other:?}"),
        }
    }

    #[test]
    fn plugin_lifecycle_context_uses_plugin_specific_settings() {
        let mut config = BmuxConfig::default();
        config
            .plugins
            .settings
            .insert("example.plugin".to_string(), "configured".into());

        let paths = ConfigPaths::new(
            std::path::PathBuf::from("/config"),
            std::path::PathBuf::from("/runtime"),
            std::path::PathBuf::from("/data"),
            std::path::PathBuf::from("/state"),
        );
        let declaration = bmux_plugin::PluginDeclaration {
            id: bmux_plugin::PluginId::new("example.plugin").expect("id should parse"),
            display_name: "Example".to_string(),
            plugin_version: "0.1.0".to_string(),
            plugin_api: bmux_plugin_sdk::VersionRange::at_least(bmux_plugin_sdk::ApiVersion::new(
                1, 0,
            )),
            native_abi: bmux_plugin_sdk::VersionRange::at_least(bmux_plugin_sdk::ApiVersion::new(
                1, 0,
            )),
            entrypoint: bmux_plugin::PluginEntrypoint::Native {
                symbol: bmux_plugin_sdk::DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
            },
            description: None,
            homepage: None,
            provider_priority: 0,
            required_capabilities: std::collections::BTreeSet::from([
                bmux_plugin_sdk::HostScope::new("bmux.commands").expect("capability should parse"),
            ]),
            provided_capabilities: std::collections::BTreeSet::from([
                bmux_plugin_sdk::HostScope::new("example.provider.write")
                    .expect("capability should parse"),
            ]),
            provided_features: std::collections::BTreeSet::new(),
            services: vec![bmux_plugin_sdk::PluginService {
                capability: bmux_plugin_sdk::HostScope::new("example.provider.write")
                    .expect("capability should parse"),
                kind: bmux_plugin_sdk::ServiceKind::Command,
                interface_id: "provider-command/v1".to_string(),
            }],
            commands: Vec::new(),
            event_subscriptions: Vec::new(),
            dependencies: Vec::new(),
            lifecycle: bmux_plugin::PluginLifecycle::default(),
        };
        let context = crate::runtime::plugin_lifecycle_context(
            &config,
            &paths,
            &declaration,
            crate::runtime::service_descriptors_from_declarations([&declaration]),
            vec![
                "bmux.commands".to_string(),
                "example.provider.write".to_string(),
            ],
            vec!["example.plugin".to_string()],
            vec!["/plugins".to_string()],
            Vec::new(),
        );
        assert_eq!(context.plugin_id, "example.plugin");
        assert_eq!(context.connection.data_dir, "/data");
        assert_eq!(
            context.required_capabilities,
            vec!["bmux.commands".to_string()]
        );
        assert_eq!(
            context.provided_capabilities,
            vec!["example.provider.write".to_string()]
        );
        assert_eq!(context.services.len(), 13);
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "config-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "storage-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "storage-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "logging-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "client-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "context-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "context-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "session-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "session-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "pane-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "pane-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "recording-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "provider-command/v1")
        );
        assert_eq!(
            context.settings.as_ref().and_then(|value| value.as_str()),
            Some("configured")
        );
    }

    #[test]
    fn plugin_command_context_includes_capability_sets() {
        let config = BmuxConfig::default();
        let paths = ConfigPaths::new(
            std::path::PathBuf::from("/config"),
            std::path::PathBuf::from("/runtime"),
            std::path::PathBuf::from("/data"),
            std::path::PathBuf::from("/state"),
        );
        let declaration = bmux_plugin::PluginDeclaration {
            id: bmux_plugin::PluginId::new("provider.plugin").expect("id should parse"),
            display_name: "Provider".to_string(),
            plugin_version: "0.1.0".to_string(),
            plugin_api: bmux_plugin_sdk::VersionRange::at_least(bmux_plugin_sdk::ApiVersion::new(
                1, 0,
            )),
            native_abi: bmux_plugin_sdk::VersionRange::at_least(bmux_plugin_sdk::ApiVersion::new(
                1, 0,
            )),
            entrypoint: bmux_plugin::PluginEntrypoint::Native {
                symbol: bmux_plugin_sdk::DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
            },
            description: None,
            homepage: None,
            provider_priority: 0,
            required_capabilities: std::collections::BTreeSet::from([
                bmux_plugin_sdk::HostScope::new("bmux.commands").expect("capability should parse"),
                bmux_plugin_sdk::HostScope::new("example.base.read")
                    .expect("capability should parse"),
            ]),
            provided_capabilities: std::collections::BTreeSet::from([
                bmux_plugin_sdk::HostScope::new("example.provider.read")
                    .expect("capability should parse"),
                bmux_plugin_sdk::HostScope::new("example.provider.write")
                    .expect("capability should parse"),
            ]),
            provided_features: std::collections::BTreeSet::new(),
            services: vec![
                bmux_plugin_sdk::PluginService {
                    capability: bmux_plugin_sdk::HostScope::new("example.provider.read")
                        .expect("capability should parse"),
                    kind: bmux_plugin_sdk::ServiceKind::Query,
                    interface_id: "provider-query/v1".to_string(),
                },
                bmux_plugin_sdk::PluginService {
                    capability: bmux_plugin_sdk::HostScope::new("example.provider.write")
                        .expect("capability should parse"),
                    kind: bmux_plugin_sdk::ServiceKind::Command,
                    interface_id: "provider-command/v1".to_string(),
                },
            ],
            commands: Vec::new(),
            event_subscriptions: Vec::new(),
            dependencies: Vec::new(),
            lifecycle: bmux_plugin::PluginLifecycle::default(),
        };

        let context = crate::runtime::plugin_command_context(
            &config,
            &paths,
            &declaration,
            "run-action",
            &["--name".to_string(), "editor".to_string()],
            crate::runtime::service_descriptors_from_declarations([&declaration]),
            vec![
                "bmux.commands".to_string(),
                "example.base.read".to_string(),
                "example.provider.read".to_string(),
                "example.provider.write".to_string(),
            ],
            vec!["provider.plugin".to_string()],
            vec!["/plugins".to_string()],
            Vec::new(),
        );

        assert_eq!(context.plugin_id, "provider.plugin");
        assert_eq!(context.command, "run-action");
        assert_eq!(
            context.required_capabilities,
            vec!["bmux.commands".to_string(), "example.base.read".to_string()]
        );
        assert_eq!(
            context.provided_capabilities,
            vec![
                "example.provider.read".to_string(),
                "example.provider.write".to_string()
            ]
        );
        assert_eq!(context.services.len(), 14);
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "config-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "storage-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "storage-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "logging-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "client-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "context-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "context-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "session-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "session-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "pane-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "pane-command/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "recording-command/v1")
        );
    }

    #[test]
    fn plugin_system_event_uses_system_kind_and_name() {
        let event = crate::runtime::plugin_system_event("server_started");
        assert_eq!(event.kind, bmux_plugin_sdk::PluginEventKind::System);
        assert_eq!(event.name, "server_started");
        assert_eq!(
            event
                .payload
                .get("product")
                .and_then(serde_json::Value::as_str),
            Some("bmux")
        );
    }

    #[test]
    fn plugin_event_from_server_event_maps_kind_and_payload() {
        let session_id = Uuid::from_u128(1);
        let event = crate::runtime::plugin_event_from_server_event(
            &bmux_client::ServerEvent::SessionCreated {
                id: session_id,
                name: Some("editor".to_string()),
            },
        )
        .expect("plugin event should build");
        let session_id_text = session_id.to_string();
        assert_eq!(event.kind, bmux_plugin_sdk::PluginEventKind::Session);
        assert_eq!(event.name, "session_created");
        assert!(event.payload.to_string().contains(&session_id_text));
    }

    #[test]
    fn built_in_handler_mapping_stays_in_sync_for_core_native_commands() {
        let command = Command::KillSession {
            target: "dev".to_string(),
            force_local: false,
        };
        assert_eq!(
            crate::runtime::built_in_handler_for_command(&command),
            crate::runtime::BuiltInHandlerId::KillSession
        );
    }

    #[test]
    fn runtime_keybindings_deep_merge_defaults_and_overrides() {
        let mut config = BmuxConfig::default();
        config.keybindings.runtime.clear();
        config
            .keybindings
            .runtime
            .insert("o".to_string(), "quit".to_string());

        let (runtime, _global, _scroll) = merged_runtime_keybindings(&config);

        assert_eq!(runtime.get("o"), Some(&"quit".to_string()));
        assert_eq!(
            runtime.get("%"),
            Some(&"split_focused_vertical".to_string())
        );
        assert_eq!(runtime.get("["), Some(&"enter_scroll_mode".to_string()));
    }

    #[test]
    fn trace_filtering_applies_family_and_pane_constraints() {
        let events = vec![
            ProtocolTraceEvent {
                timestamp_ms: 1,
                pane_id: Some(1),
                profile: "xterm".to_string(),
                family: "csi".to_string(),
                name: "csi_primary_da".to_string(),
                direction: ProtocolDirection::Query,
                raw_hex: "1b5b63".to_string(),
                decoded: "\u{1b}[c".to_string(),
            },
            ProtocolTraceEvent {
                timestamp_ms: 2,
                pane_id: Some(2),
                profile: "xterm".to_string(),
                family: "osc".to_string(),
                name: "osc_color_query".to_string(),
                direction: ProtocolDirection::Reply,
                raw_hex: "1b5d31303b3f".to_string(),
                decoded: "...".to_string(),
            },
            ProtocolTraceEvent {
                timestamp_ms: 3,
                pane_id: Some(2),
                profile: "xterm".to_string(),
                family: "csi".to_string(),
                name: "csi_primary_da".to_string(),
                direction: ProtocolDirection::Reply,
                raw_hex: "1b5b3f313b3263".to_string(),
                decoded: "...".to_string(),
            },
        ];

        let by_family = filter_trace_events(&events, Some(TraceFamily::Csi), None, 50);
        assert_eq!(by_family.len(), 2);

        let by_pane = filter_trace_events(&events, None, Some(2), 50);
        assert_eq!(by_pane.len(), 2);

        let both = filter_trace_events(&events, Some(TraceFamily::Csi), Some(2), 50);
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].timestamp_ms, 3);
    }

    #[test]
    fn destructive_op_error_formats_session_policy_guidance() {
        let message = crate::runtime::format_destructive_op_error(
            "session",
            ClientError::ServerError {
                code: ErrorCode::InvalidRequest,
                message: "session policy denied for this operation".to_string(),
            },
            false,
        );

        assert!(message.contains("not permitted by current session policy"));
    }

    #[test]
    fn destructive_op_error_formats_force_local_guidance() {
        let message = crate::runtime::format_destructive_op_error(
            "window",
            ClientError::ServerError {
                code: ErrorCode::InvalidRequest,
                message: "force-local is only allowed for the server control principal".to_string(),
            },
            true,
        );

        assert!(message.contains("--force-local"));
        assert!(message.contains("bmux server whoami-principal"));
    }

    #[test]
    fn format_plugin_command_run_error_adds_policy_hint_when_denied() {
        let error = anyhow::anyhow!("session policy denied for this operation");
        let message =
            crate::runtime::format_plugin_command_run_error("bmux.windows", "kill", &error);
        assert!(message.contains("failed running plugin command 'bmux.windows:kill'"));
        assert!(message.contains("operation denied by an active policy provider"));
        assert!(message.contains("authorized principal"));
    }

    #[test]
    fn format_plugin_command_run_error_keeps_generic_failures_without_hint() {
        let error = anyhow::anyhow!("unsupported service operation");
        let message =
            crate::runtime::format_plugin_command_run_error("bmux.permissions", "grant", &error);
        assert!(message.contains("failed running plugin command 'bmux.permissions:grant'"));
        assert!(!message.contains("operation denied by session policy"));
    }

    #[test]
    fn unknown_external_command_message_points_to_plugin_list_help() {
        let message = crate::runtime::unknown_external_command_message(&[
            "session".to_string(),
            "roles".to_string(),
        ]);
        assert!(message.contains("unknown command 'session roles'"));
        assert!(message.contains("bmux plugin list"));
    }

    #[test]
    fn format_plugin_not_found_message_lists_available_plugins() {
        let message = crate::runtime::format_plugin_not_found_message(
            "missing.plugin",
            &["bmux.windows".to_string(), "bmux.permissions".to_string()],
        );
        assert!(message.contains("plugin 'missing.plugin' was not found"));
        assert!(message.contains("bmux.windows, bmux.permissions"));
    }

    #[test]
    fn format_plugin_not_found_message_handles_empty_registry() {
        let empty: [&str; 0] = [];
        let message = crate::runtime::format_plugin_not_found_message("missing.plugin", &empty);
        assert_eq!(message, "plugin 'missing.plugin' was not found");
    }

    #[test]
    fn format_plugin_not_enabled_message_points_to_plugins_enabled() {
        let message = crate::runtime::format_plugin_not_enabled_message("bmux.windows");
        assert!(message.contains("plugin 'bmux.windows' is not enabled"));
        assert!(message.contains("plugins.disabled"));
        assert!(message.contains("plugins.enabled"));
    }

    #[test]
    fn format_plugin_argument_validation_error_adds_help_hint_for_missing_required() {
        let error = anyhow::anyhow!("missing required option '--session'");
        let message = crate::runtime::format_plugin_argument_validation_error(
            &["session".to_string(), "roles".to_string()],
            &error,
        );
        assert!(message.contains("failed validating plugin command arguments for 'session roles'"));
        assert!(message.contains("missing required option '--session'"));
        assert!(message.contains("--help"));
    }

    #[test]
    fn format_plugin_argument_validation_error_keeps_non_required_errors_without_hint() {
        let error = anyhow::anyhow!("unknown option '--wat'");
        let message = crate::runtime::format_plugin_argument_validation_error(
            &["session".to_string(), "roles".to_string()],
            &error,
        );
        assert!(message.contains("failed validating plugin command arguments for 'session roles'"));
        assert!(message.contains("unknown option '--wat'"));
        assert!(!message.contains("--help"));
    }

    #[test]
    fn plugin_fallback_retarget_context_id_returns_changed_context_when_no_effect_applied() {
        let before = Some(Uuid::from_u128(1));
        let after = Some(Uuid::from_u128(2));
        let attached = Some(Uuid::from_u128(1));

        assert_eq!(
            crate::runtime::plugin_fallback_retarget_context_id(before, after, attached, false),
            after
        );
    }

    #[test]
    fn plugin_fallback_retarget_context_id_ignores_when_outcome_already_applied() {
        let before = Some(Uuid::from_u128(1));
        let after = Some(Uuid::from_u128(2));
        let attached = Some(Uuid::from_u128(2));

        assert_eq!(
            crate::runtime::plugin_fallback_retarget_context_id(before, after, attached, true),
            None
        );
    }

    #[test]
    fn plugin_fallback_new_context_id_returns_single_new_context() {
        let before = [Uuid::from_u128(1)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let after = [Uuid::from_u128(1), Uuid::from_u128(2)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            crate::runtime::plugin_fallback_new_context_id(
                Some(&before),
                Some(&after),
                Some(Uuid::from_u128(1)),
                Some(Uuid::from_u128(1)),
                false,
            ),
            Some(Uuid::from_u128(2))
        );
    }

    #[test]
    fn plugin_fallback_new_context_id_prefers_after_context_when_multiple_new() {
        let before = [Uuid::from_u128(1)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let after = [Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            crate::runtime::plugin_fallback_new_context_id(
                Some(&before),
                Some(&after),
                Some(Uuid::from_u128(1)),
                Some(Uuid::from_u128(3)),
                false,
            ),
            Some(Uuid::from_u128(3))
        );
    }

    #[test]
    fn plugin_fallback_new_context_id_ignores_when_outcome_applied() {
        let before = [Uuid::from_u128(1)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let after = [Uuid::from_u128(1), Uuid::from_u128(2)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            crate::runtime::plugin_fallback_new_context_id(
                Some(&before),
                Some(&after),
                Some(Uuid::from_u128(1)),
                Some(Uuid::from_u128(2)),
                true,
            ),
            None
        );
    }

    #[test]
    fn host_kernel_effect_capture_records_select_context_from_select_response() {
        crate::runtime::begin_host_kernel_effect_capture();
        let context_id = Uuid::from_u128(42);
        crate::runtime::maybe_record_host_kernel_effect(
            &bmux_ipc::Request::SelectContext {
                selector: bmux_ipc::ContextSelector::ById(context_id),
            },
            &bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ContextSelected {
                context: bmux_ipc::ContextSummary {
                    id: context_id,
                    name: Some("ctx".to_string()),
                    attributes: std::collections::BTreeMap::new(),
                },
            }),
        );
        let captured = crate::runtime::finish_host_kernel_effect_capture();
        assert_eq!(
            captured,
            vec![PluginCommandEffect::SelectContext { context_id }]
        );
    }

    #[test]
    fn host_kernel_effect_capture_ignores_non_context_responses() {
        crate::runtime::begin_host_kernel_effect_capture();
        crate::runtime::maybe_record_host_kernel_effect(
            &bmux_ipc::Request::ListSessions,
            &bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::SessionList {
                sessions: Vec::new(),
            }),
        );
        let captured = crate::runtime::finish_host_kernel_effect_capture();
        assert!(captured.is_empty());
    }
}
