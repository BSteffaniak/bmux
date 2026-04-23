use anyhow::{Context, Result};
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_plugin::{
    PluginManifest, PluginRegistry, load_registered_plugin as load_native_registered_plugin,
};
use bmux_plugin_sdk::{
    CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION, HostConnectionInfo, HostMetadata,
    HostScope, NativeCommandContext, NativeLifecycleContext, PluginCommandOutcome, PluginEvent,
    PluginEventKind, RegisteredService,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

use super::{
    KernelClientFactory, available_capability_providers, available_service_descriptors,
    connect_raw, core_provided_capabilities, enter_host_kernel_client_factory,
    enter_host_kernel_connection, host_kernel_bridge, map_cli_client_error,
    plugin_commands::PluginCommandRegistry, plugin_host, server_event_name,
    service_descriptors_from_declarations,
};

#[derive(Clone)]
struct RuntimeCommandState {
    config: BmuxConfig,
    paths: ConfigPaths,
    registry: Arc<PluginRegistry>,
    enabled_plugins: Vec<String>,
    available_capability_providers: BTreeMap<HostScope, bmux_plugin::CapabilityProvider>,
    plugin_search_roots: Vec<String>,
    registered_plugin_infos: Vec<bmux_plugin_sdk::RegisteredPluginInfo>,
}

thread_local! {
    static RUNTIME_COMMAND_STATE_CACHE: RefCell<Option<RuntimeCommandState>> = const { RefCell::new(None) };
    static LOADED_PLUGIN_CACHE: RefCell<BTreeMap<String, Rc<bmux_plugin::LoadedPlugin>>> = const { RefCell::new(BTreeMap::new()) };
}

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
    .with_typed_services(typed_service_registry_snapshot())
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
/// binary only includes the plugin code the user opted into (all bundled by
/// default via the `bundled-plugins` feature).
///
/// To add a new bundled plugin, append one three-line entry here -- no need to
/// touch two separate functions.
macro_rules! declare_bundled_plugins {
    ($(
        feature = $feature:literal,
        manifest = $manifest:expr,
        plugin_type = $ty:ty;
    )*) => {
        #[allow(unused_variables, clippy::missing_const_for_fn)]
        fn register_static_bundled_plugins(registry: &mut PluginRegistry) {
            $(
                #[cfg(feature = $feature)]
                if let Err(e) = registry.register_bundled_manifest($manifest) {
                    let plugin_id = bundled_manifest_plugin_id($manifest)
                        .unwrap_or_else(|| "<unknown-plugin-id>".to_string());
                    tracing::warn!("failed to register bundled plugin '{plugin_id}': {e}");
                }
                #[cfg(feature = $feature)]
                if let Some(plugin_id) = bundled_manifest_plugin_id($manifest) {
                    let vtable = bmux_plugin_sdk::bundled_plugin_vtable!($ty, $manifest);
                    bmux_plugin::register_static_vtable(&plugin_id, vtable);
                }
            )*
        }

        #[allow(unused_variables, clippy::missing_const_for_fn)]
        fn static_bundled_vtable(plugin_id: &str) -> Option<bmux_plugin_sdk::StaticPluginVtable> {
            $(
                #[cfg(feature = $feature)]
                if bundled_manifest_plugin_id($manifest)
                    .as_deref()
                    .is_some_and(|manifest_plugin_id| manifest_plugin_id == plugin_id)
                {
                    return Some(bmux_plugin_sdk::bundled_plugin_vtable!($ty, $manifest));
                }
            )*
            None
        }
    };
}

declare_bundled_plugins! {
    feature = "bundled-plugin-clients",
    manifest = include_str!("../../../../plugins/clients-plugin/plugin.toml"),
    plugin_type = bmux_clients_plugin::ClientsPlugin;

    feature = "bundled-plugin-clipboard",
    manifest = include_str!("../../../../plugins/clipboard-plugin/plugin.toml"),
    plugin_type = bmux_clipboard_plugin::ClipboardPlugin;

    feature = "bundled-plugin-cluster",
    manifest = include_str!("../../../../plugins/cluster-plugin/plugin.toml"),
    plugin_type = bmux_cluster_plugin::ClusterPlugin;

    feature = "bundled-plugin-contexts",
    manifest = include_str!("../../../../plugins/contexts-plugin/plugin.toml"),
    plugin_type = bmux_contexts_plugin::ContextsPlugin;

    feature = "bundled-plugin-control-catalog",
    manifest = include_str!("../../../../plugins/control-catalog-plugin/plugin.toml"),
    plugin_type = bmux_control_catalog_plugin::ControlCatalogPlugin;

    feature = "bundled-plugin-performance",
    manifest = include_str!("../../../../plugins/performance-plugin/plugin.toml"),
    plugin_type = bmux_performance_plugin::PerformancePlugin;

    feature = "bundled-plugin-permissions",
    manifest = include_str!("../../../../plugins/permissions-plugin/plugin.toml"),
    plugin_type = bmux_permissions_plugin::PermissionsPlugin;

    feature = "bundled-plugin-cli",
    manifest = include_str!("../../../../plugins/plugin-cli-plugin/plugin.toml"),
    plugin_type = bmux_plugin_cli_plugin::PluginCliPlugin;

    feature = "bundled-plugin-prompted-actions",
    manifest = include_str!("../../../../plugins/prompted-actions-plugin/plugin.toml"),
    plugin_type = bmux_prompted_actions_plugin::PromptedActionsPlugin;

    feature = "bundled-plugin-recording",
    manifest = include_str!("../../../../plugins/recording-plugin/plugin.toml"),
    plugin_type = bmux_recording_plugin::RecordingPlugin;

    feature = "bundled-plugin-sessions",
    manifest = include_str!("../../../../plugins/sessions-plugin/plugin.toml"),
    plugin_type = bmux_sessions_plugin::SessionsPlugin;

    feature = "bundled-plugin-pane-runtime",
    manifest = include_str!("../../../../plugins/pane-runtime-plugin/plugin.toml"),
    plugin_type = bmux_pane_runtime_plugin::PaneRuntimePlugin;

    feature = "bundled-plugin-snapshot",
    manifest = include_str!("../../../../plugins/snapshot-plugin/plugin.toml"),
    plugin_type = bmux_snapshot_plugin::SnapshotPlugin;

    feature = "bundled-plugin-windows",
    manifest = include_str!("../../../../plugins/windows-plugin/plugin.toml"),
    plugin_type = bmux_windows_plugin::WindowsPlugin;

    feature = "bundled-plugin-decoration",
    manifest = include_str!("../../../../plugins/decoration-plugin/plugin.toml"),
    plugin_type = bmux_decoration_plugin::DecorationPlugin;
}

fn bundled_manifest_plugin_id(manifest: &str) -> Option<String> {
    bmux_plugin::PluginManifest::from_toml_str(manifest)
        .ok()
        .map(|parsed| parsed.id)
}

/// Load a registered plugin, using the static vtable path for bundled plugins
/// and the dynamic `dlopen` path for everything else.
#[allow(clippy::result_large_err)] // Plugin error types carry context payloads
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

pub fn scan_available_plugins(config: &BmuxConfig, paths: &ConfigPaths) -> Result<PluginRegistry> {
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
                    ) && !entry_path.exists()
                        && workspace_bundled_root
                            .as_ref()
                            .is_some_and(|root| report.search_root == *root)
                        && let Ok(executable) = std::env::current_exe()
                        && let Some(executable_dir) = executable.parent()
                        && let Some(entry) = manifest.entry.as_ref()
                    {
                        let executable_candidate = executable_dir.join(entry);
                        if executable_candidate.exists() {
                            manifest.entry = Some(executable_candidate);
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

fn runtime_command_state() -> Result<RuntimeCommandState> {
    RUNTIME_COMMAND_STATE_CACHE.with(|slot| {
        if let Some(state) = slot.borrow().clone() {
            return Ok(state);
        }

        let config = BmuxConfig::load()?;
        let paths = ConfigPaths::default();
        let registry = Arc::new(scan_available_plugins(&config, &paths)?);
        let enabled_plugins = effective_enabled_plugins(&config, &registry);
        let available_capability_providers = available_capability_providers(&config, &registry)?;
        let plugin_search_roots = resolve_plugin_search_paths(&config, &paths)?
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let registered_plugin_infos = registered_plugin_infos_from_registry(&registry);
        let state = RuntimeCommandState {
            config,
            paths,
            registry,
            enabled_plugins,
            available_capability_providers,
            plugin_search_roots,
            registered_plugin_infos,
        };
        *slot.borrow_mut() = Some(state.clone());
        Ok(state)
    })
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

pub fn bundled_plugin_roots() -> Vec<PathBuf> {
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

pub fn registered_plugin_entry_exists(plugin: &bmux_plugin::RegisteredPlugin) -> bool {
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
pub fn discover_bundled_plugin_ids() -> Vec<String> {
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
        validate_plugin_routing_policy(config, registry)?;
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
    validate_plugin_routing_policy(config, registry)?;

    Ok(())
}

fn validate_plugin_routing_policy(config: &BmuxConfig, registry: &PluginRegistry) -> Result<()> {
    let mut command_config = config.clone();
    command_config.plugins.enabled = effective_enabled_plugins(config, registry);
    let command_registry = PluginCommandRegistry::build(&command_config, registry)
        .context("failed building plugin CLI command registry")?;

    match config.plugins.routing.conflict_mode {
        bmux_config::PluginRoutingConflictMode::FailStartup => {}
    }

    for claim in &config.plugins.routing.required_namespaces {
        validate_required_namespace_claim(claim, &command_registry)?;
    }
    for claim in &config.plugins.routing.required_paths {
        validate_required_path_claim(claim, &command_registry)?;
    }

    Ok(())
}

fn validate_required_namespace_claim(
    claim: &bmux_config::RequiredNamespaceClaim,
    command_registry: &PluginCommandRegistry,
) -> Result<()> {
    let namespace = claim.namespace.trim();
    if namespace.is_empty() {
        anyhow::bail!("plugins.routing.required_namespaces.namespace must not be empty");
    }
    let owner = command_registry
        .owner_for_namespace(namespace)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "required namespace claim '{namespace}' is not owned by any enabled plugin"
            )
        })?;
    if let Some(expected_owner) = claim.owner.as_deref()
        && owner != expected_owner
    {
        anyhow::bail!(
            "required namespace claim '{namespace}' must be owned by plugin '{expected_owner}' (actual owner '{owner}')"
        );
    }
    Ok(())
}

fn validate_required_path_claim(
    claim: &bmux_config::RequiredPathClaim,
    command_registry: &PluginCommandRegistry,
) -> Result<()> {
    if claim.path.is_empty() {
        anyhow::bail!("plugins.routing.required_paths.path must not be empty");
    }
    if claim.path.iter().any(|segment| segment.trim().is_empty()) {
        anyhow::bail!(
            "plugins.routing.required_paths.path must not contain empty command segments"
        );
    }

    let owner = command_registry
        .owner_for_path(&claim.path)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "required path claim '{}' is not owned by any enabled plugin",
                claim.path.join(" ")
            )
        })?;
    if let Some(expected_owner) = claim.owner.as_deref()
        && owner != expected_owner
    {
        anyhow::bail!(
            "required path claim '{}' must be owned by plugin '{}' (actual owner '{}')",
            claim.path.join(" "),
            expected_owner,
            owner
        );
    }
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

#[allow(clippy::too_many_arguments)]
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

#[allow(clippy::too_many_arguments)]
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
        caller_client_id: None,
        host_kernel_bridge: Some(bmux_plugin_sdk::HostKernelBridge::from_fn(
            host_kernel_bridge,
        )),
    }
}

pub(super) fn plugin_system_event(name: &str) -> PluginEvent {
    PluginEvent {
        kind: PluginEventKind::from_owned(format!("bmux.core/{name}")),
        payload: serde_json::json!({
            "product": "bmux",
            "version": env!("CARGO_PKG_VERSION"),
        }),
    }
}

thread_local! {
    /// Shared map of typed service handles published by loaded plugins.
    ///
    /// Built once during `activate_loaded_plugins`; consumers that want
    /// typed dispatch (skipping byte-encoded serialization) read a
    /// snapshot via [`typed_service_registry_snapshot`] and attach it
    /// to a [`plugin_host::CliPluginHost`] so
    /// `PluginHost::resolve_typed_service` surfaces the handle.
    static TYPED_SERVICE_REGISTRY: std::cell::RefCell<
        std::sync::Arc<
            std::collections::BTreeMap<
                bmux_plugin_sdk::TypedServiceKey,
                bmux_plugin_sdk::TypedServiceHandle,
            >,
        >,
    > = std::cell::RefCell::new(std::sync::Arc::new(std::collections::BTreeMap::new()));

    /// Per-thread readiness tracker observing every loaded plugin's
    /// declared ready signals. Each plugin's signals are declared as
    /// `Pending` during `install_typed_service_registry` and flipped
    /// to `Ready` after the plugin's `activate` call returns.
    /// Subsystems (e.g. the attach render loop) consult the tracker
    /// via [`ready_tracker_snapshot`] to sequence startup against
    /// plugin availability.
    static READY_TRACKER: std::cell::RefCell<bmux_plugin_sdk::ReadyTracker> =
        std::cell::RefCell::new(bmux_plugin_sdk::ReadyTracker::new());
}

/// Harvest every loaded plugin's typed services and install the
/// combined map on the thread-local registry. Also declares each
/// plugin's ready signals on the thread-local [`ReadyTracker`] so
/// consumers can wait for specific signals to flip.
fn install_typed_service_registry(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
    config: &BmuxConfig,
    paths: &ConfigPaths,
) {
    // Build a bridge that plugins may stash inside their typed
    // service handles. It shares the same dispatch function as the
    // per-activation bridge in `plugin_lifecycle_context`, so host
    // calls from trait methods reach the same kernel routing as
    // host calls from `invoke_service` / `run_command`.
    let bridge = bmux_plugin_sdk::HostKernelBridge::from_fn(host_kernel_bridge);

    // Shared data that every plugin's typed-services context references
    // read-only. Collected once and borrowed per plugin so typed handles
    // can construct standalone `ServiceCaller` wrappers that drive
    // `call_service_raw` without needing a per-activation
    // `NativeLifecycleContext` on every typed call.
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
    let search_roots = resolve_plugin_search_paths(config, paths)
        .map(|paths| {
            paths
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let host_metadata = plugin_host_metadata();
    let host_connection = bmux_plugin_sdk::HostConnectionInfo {
        config_dir: paths.config_dir.to_string_lossy().into_owned(),
        config_dir_candidates: paths
            .config_dir_candidates()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        runtime_dir: paths.runtime_dir.to_string_lossy().into_owned(),
        data_dir: paths.data_dir.to_string_lossy().into_owned(),
        state_dir: paths.state_dir.to_string_lossy().into_owned(),
    };
    let plugin_settings_map: std::collections::BTreeMap<String, toml::Value> =
        config.plugins.settings.clone();

    let mut map: std::collections::BTreeMap<
        bmux_plugin_sdk::TypedServiceKey,
        bmux_plugin_sdk::TypedServiceHandle,
    > = std::collections::BTreeMap::new();
    for plugin in loaded_plugins {
        let required_caps: Vec<String> = plugin
            .declaration
            .required_capabilities
            .iter()
            .map(ToString::to_string)
            .collect();
        let provided_caps: Vec<String> = plugin
            .declaration
            .provided_capabilities
            .iter()
            .map(ToString::to_string)
            .collect();
        let context = bmux_plugin_sdk::TypedServiceRegistrationContext {
            plugin_id: plugin.declaration.id.as_str(),
            host_kernel_bridge: Some(&bridge),
            required_capabilities: &required_caps,
            provided_capabilities: &provided_caps,
            services: &available_services,
            available_capabilities: &available_capabilities,
            enabled_plugins: &enabled_plugins,
            plugin_search_roots: &search_roots,
            host: &host_metadata,
            connection: &host_connection,
            plugin_settings_map: &plugin_settings_map,
        };
        for (key, handle) in plugin.collect_typed_services(context).into_entries() {
            map.insert(key, handle);
        }
    }
    TYPED_SERVICE_REGISTRY.with(|cell| {
        *cell.borrow_mut() = std::sync::Arc::new(map);
    });

    READY_TRACKER.with(|cell| {
        let tracker = cell.borrow();
        for plugin in loaded_plugins {
            tracker.declare(
                plugin.declaration.id.as_str(),
                &plugin.declaration.ready_signals,
            );
        }
    });
}

/// Flip every ready signal declared by `plugin` to
/// [`bmux_plugin_sdk::ReadyStatus::Ready`]. Called after the plugin's
/// `activate` returns `Ok` to signal that any subsystem waiting on one
/// of its signals can proceed. Plugins that need more granular control
/// (e.g. waiting on async work beyond activation) will be extended
/// with an explicit `mark_ready` host-kernel bridge operation later.
fn mark_plugin_ready_signals(plugin: &bmux_plugin::LoadedPlugin) {
    READY_TRACKER.with(|cell| {
        let tracker = cell.borrow();
        for signal in &plugin.declaration.ready_signals {
            tracker.mark_ready(plugin.declaration.id.as_str(), &signal.name);
        }
    });
}

/// Snapshot the current typed service registry. Consumers that wire a
/// [`plugin_host::CliPluginHost`] pass this into
/// [`plugin_host::CliPluginHost::with_typed_services`] so typed
/// resolution surfaces any typed handle the loaded plugins registered.
#[must_use]
pub(super) fn typed_service_registry_snapshot() -> std::sync::Arc<
    std::collections::BTreeMap<
        bmux_plugin_sdk::TypedServiceKey,
        bmux_plugin_sdk::TypedServiceHandle,
    >,
> {
    TYPED_SERVICE_REGISTRY.with(|cell| std::sync::Arc::clone(&cell.borrow()))
}

/// Resolve the typed `windows-commands` service handle from the current
/// registry, if the windows plugin is loaded and registered typed
/// services.
///
/// Returns `None` when the windows plugin is absent, when it registered
/// only byte-encoded services, or when the capability identifier is
/// malformed. The typed path is a strict opt-in — callers that must
/// work regardless of plugin state should fall back to the existing
/// byte-encoded `bmux_client` methods.
#[must_use]
#[allow(dead_code)] // Consumed by attach-flow routing changes landing in a follow-up.
pub(super) fn resolve_windows_commands_service() -> Option<
    std::sync::Arc<
        dyn bmux_windows_plugin_api::windows_commands::WindowsCommandsService + Send + Sync,
    >,
> {
    let write_cap = bmux_plugin_sdk::HostScope::new("bmux.windows.write").ok()?;
    let registry = typed_service_registry_snapshot();
    let handle = registry.get(&(
        write_cap,
        bmux_plugin_sdk::ServiceKind::Command,
        bmux_windows_plugin_api::windows_commands::INTERFACE_ID.to_string(),
    ))?;
    handle
        .provider_as_trait::<
            dyn bmux_windows_plugin_api::windows_commands::WindowsCommandsService + Send + Sync,
        >()
        .ok()
}

/// Resolve the typed `windows-state` service handle from the current
/// registry. Mirrors [`resolve_windows_commands_service`] for the
/// read-only query interface.
#[must_use]
#[allow(dead_code)] // Consumed by attach-flow routing changes landing in a follow-up.
pub(super) fn resolve_windows_state_service() -> Option<
    std::sync::Arc<dyn bmux_windows_plugin_api::windows_state::WindowsStateService + Send + Sync>,
> {
    let read_cap = bmux_plugin_sdk::HostScope::new("bmux.windows.read").ok()?;
    let registry = typed_service_registry_snapshot();
    let handle = registry.get(&(
        read_cap,
        bmux_plugin_sdk::ServiceKind::Query,
        bmux_windows_plugin_api::windows_state::INTERFACE_ID.to_string(),
    ))?;
    handle
        .provider_as_trait::<
            dyn bmux_windows_plugin_api::windows_state::WindowsStateService + Send + Sync,
        >()
        .ok()
}

/// Snapshot the current [`bmux_plugin_sdk::ReadyTracker`]. Cheap clone
/// (internal `Arc`); consumers observe signal state via
/// [`bmux_plugin_sdk::ReadyTracker::is_ready`] /
/// [`bmux_plugin_sdk::ReadyTracker::await_ready`] without coupling to
/// the plugin-runtime module's thread-local cell.
#[must_use]
#[allow(dead_code)] // Consumed by attach-render ready-gate wiring landing in a follow-up.
pub(super) fn ready_tracker_snapshot() -> bmux_plugin_sdk::ReadyTracker {
    READY_TRACKER.with(|cell| cell.borrow().clone())
}

/// Populate a [`bmux_attach_pipeline::scene_cache::SharedSceneCache`]
/// with the decoration plugin's current [`bmux_scene_protocol::scene_protocol::DecorationScene`].
///
/// Resolves the typed `decoration-state` service from the thread-local
/// typed-service registry, invokes `scene_snapshot`, and writes the
/// result through the revision-guarded cache update. Silently no-ops
/// when no decoration plugin is registered, when the typed handle is
/// not present (e.g. the plugin is loaded but opted out of typed
/// dispatch), or when the plugin's `scene_snapshot` produces a stale
/// revision.
///
/// The push-based event subscription path will later invalidate the
/// cache incrementally; this helper handles the cold-start case where
/// the render loop needs a scene before any event has fired.
pub(super) fn prime_decoration_scene_cache(
    cache: &bmux_attach_pipeline::scene_cache::SharedSceneCache,
) {
    let Ok(read_cap) = bmux_plugin_sdk::HostScope::new("bmux.decoration.read") else {
        return;
    };
    let registry = typed_service_registry_snapshot();
    let Some(handle) = registry.get(&(
        read_cap,
        bmux_plugin_sdk::ServiceKind::Query,
        "decoration-state".to_string(),
    )) else {
        return;
    };
    let Ok(service) = handle.provider_as_trait::<
        dyn bmux_decoration_plugin_api::decoration_state::DecorationStateService + Send + Sync,
    >() else {
        return;
    };
    let scene = block_on_future(service.scene_snapshot());
    if let Ok(mut guard) = cache.write() {
        guard.set_scene(scene);
    }
}

/// Push a pane's current `rect` + `content_rect` to the decoration
/// plugin via its typed `notify-pane-geometry` command. Silently
/// no-ops when the decoration plugin is not loaded (mirrors the
/// cold-start policy of [`prime_decoration_scene_cache`]).
///
/// The decoration plugin is a write-capability holder for this
/// command. Core invokes it through the generic typed-dispatch
/// registry, matching the existing precedent for the read path.
pub(super) fn push_decoration_pane_geometry(
    pane_id: uuid::Uuid,
    rect: bmux_scene_protocol::scene_protocol::Rect,
    content_rect: bmux_scene_protocol::scene_protocol::Rect,
) {
    let Ok(write_cap) = bmux_plugin_sdk::HostScope::new("bmux.decoration.write") else {
        return;
    };
    let registry = typed_service_registry_snapshot();
    let Some(handle) = registry.get(&(
        write_cap,
        bmux_plugin_sdk::ServiceKind::Command,
        "decoration-state".to_string(),
    )) else {
        return;
    };
    let Ok(service) = handle.provider_as_trait::<
        dyn bmux_decoration_plugin_api::decoration_state::DecorationStateService + Send + Sync,
    >() else {
        return;
    };
    let geometry = bmux_decoration_plugin_api::decoration_state::PaneGeometry {
        pane_id,
        rect,
        content_rect,
    };
    let _ = block_on_future(service.notify_pane_geometry(geometry));
}

/// Drop any decoration state the plugin is holding for `pane_id`.
/// Called by the attach runtime when a pane disappears from the
/// observed layout (close / session detach). Silently no-ops when
/// the decoration plugin is not loaded.
pub(super) fn forget_decoration_pane(pane_id: uuid::Uuid) {
    let Ok(write_cap) = bmux_plugin_sdk::HostScope::new("bmux.decoration.write") else {
        return;
    };
    let registry = typed_service_registry_snapshot();
    let Some(handle) = registry.get(&(
        write_cap,
        bmux_plugin_sdk::ServiceKind::Command,
        "decoration-state".to_string(),
    )) else {
        return;
    };
    let Ok(service) = handle.provider_as_trait::<
        dyn bmux_decoration_plugin_api::decoration_state::DecorationStateService + Send + Sync,
    >() else {
        return;
    };
    let _ = block_on_future(service.forget_pane(pane_id));
}

/// Minimal single-threaded executor for driving a typed-dispatch
/// future to completion from synchronous code.
///
/// Typed service method futures returned by the BPDL-generated trait
/// are `Pin<Box<dyn Future + Send>>`. Runtime consumers that live
/// inside synchronous call paths (e.g. the attach startup sequence
/// priming the scene cache) can use this helper without pulling in a
/// full tokio runtime. The futures produced by the decoration plugin's
/// typed service are `async move { ... }` blocks that never suspend,
/// so `Poll::Pending` shouldn't be reachable in practice; if it is
/// observed we spin briefly and yield rather than hanging the thread.
fn block_on_future<T>(
    fut: std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + '_>>,
) -> T {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    struct NoopWake;
    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    let waker = Waker::from(Arc::new(NoopWake));
    let mut cx = Context::from_waker(&waker);
    let mut pinned = fut;
    loop {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

pub(super) fn activate_loaded_plugins(
    loaded_plugins: &[bmux_plugin::LoadedPlugin],
    config: &BmuxConfig,
    paths: &ConfigPaths,
) -> Result<()> {
    // Snapshot the typed services each plugin exposes before we
    // activate anyone; bundled plugins can have their typed handles
    // harvested without side effects, and consumers that resolve a
    // typed service observe a stable registry across the lifetime of
    // the attach session.
    install_typed_service_registry(loaded_plugins, config, paths);

    let mut activated: Vec<&bmux_plugin::LoadedPlugin> = Vec::new();
    let connection_info = HostConnectionInfo {
        config_dir: paths.config_dir.to_string_lossy().into_owned(),
        config_dir_candidates: paths
            .config_dir_candidates()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
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
        mark_plugin_ready_signals(plugin);
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
        config_dir_candidates: paths
            .config_dir_candidates()
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
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
    event: &PluginEvent,
) -> Result<()> {
    for plugin in loaded_plugins {
        let _ = plugin.dispatch_event(event).with_context(|| {
            format!(
                "failed dispatching plugin event '{}' to '{}'",
                event.kind.as_str(),
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

    let client = loop {
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

    // Upgrade to streaming client for server-push event delivery.
    let Ok(mut streaming_client) = bmux_client::StreamingBmuxClient::from_client(client) else {
        // Fallback: if upgrade fails (e.g., bridge stream), just return.
        return Ok(());
    };
    streaming_client
        .subscribe_events()
        .await
        .map_err(map_cli_client_error)?;
    streaming_client
        .enable_event_push()
        .await
        .map_err(map_cli_client_error)?;

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return Ok(());
                }
            }
            event = streaming_client.event_receiver().recv() => {
                let Some(event) = event else {
                    return Ok(()); // server disconnected
                };
                // Skip high-frequency pane output notifications for plugins.
                if matches!(
                    event,
                    bmux_client::ServerEvent::PaneOutputAvailable { .. }
                        | bmux_client::ServerEvent::PaneOutput { .. }
                ) {
                    continue;
                }
                dispatch_loaded_plugin_event(loaded_plugins, &plugin_event_from_server_event(&event)?)?;
            }
        }
    }
}

pub(super) fn plugin_event_from_server_event(
    event: &bmux_client::ServerEvent,
) -> Result<PluginEvent> {
    Ok(PluginEvent {
        kind: plugin_event_kind_from_server_event(event),
        payload: serde_json::to_value(event).context("failed encoding server event payload")?,
    })
}

/// Translate a legacy `ServerEvent` into a namespaced [`PluginEventKind`].
///
/// The returned kind uses the `bmux.core/<event-name>` namespace for now so
/// every legacy server-emitted event slots into the generic kind scheme.
/// Once the server-side domain concepts are owned by plugins (sessions /
/// windows / contexts / clients), each plugin will publish its own event
/// streams with its own kinds and this shim disappears.
pub(super) fn plugin_event_kind_from_server_event(
    event: &bmux_client::ServerEvent,
) -> PluginEventKind {
    PluginEventKind::from_owned(format!("bmux.core/{}", server_event_name(event)))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Status clamped to 0..255 before cast
pub(super) async fn run_plugin_command(
    plugin_id: &str,
    command_name: &str,
    args: &[String],
) -> Result<u8> {
    let status = run_plugin_command_internal(plugin_id, command_name, args, None)?.status;
    Ok(status.clamp(0, i32::from(u8::MAX)) as u8)
}

pub(super) fn run_plugin_keybinding_command(
    plugin_id: &str,
    command_name: &str,
    args: &[String],
    kernel_client_factory: Option<&KernelClientFactory>,
) -> Result<PluginCommandExecution> {
    run_plugin_command_internal(plugin_id, command_name, args, kernel_client_factory)
}

#[derive(Debug, Clone)]
pub(super) struct PluginCommandPolicyHints {
    pub(super) execution: bmux_plugin_sdk::CommandExecutionKind,
    pub(super) execution_class: bmux_plugin::PluginExecutionClass,
    pub(super) required_capabilities: Vec<HostScope>,
}

pub(super) fn plugin_command_policy_hints(
    plugin_id: &str,
    command_name: &str,
) -> Result<PluginCommandPolicyHints> {
    let state = runtime_command_state()?;
    let registry = &state.registry;
    let available = registry.plugin_ids();
    let plugin = registry
        .get(plugin_id)
        .with_context(|| format_plugin_not_found_message(plugin_id, &available))?;
    if !state
        .enabled_plugins
        .iter()
        .any(|enabled| enabled == plugin_id)
    {
        anyhow::bail!(format_plugin_not_enabled_message(plugin_id));
    }

    let command = plugin
        .declaration
        .commands
        .iter()
        .find(|entry| entry.name == command_name)
        .ok_or_else(|| {
            anyhow::anyhow!("plugin '{plugin_id}' does not declare command '{command_name}'")
        })?;

    Ok(PluginCommandPolicyHints {
        execution: command.execution.clone(),
        execution_class: plugin.declaration.execution_class,
        required_capabilities: plugin
            .declaration
            .required_capabilities
            .iter()
            .cloned()
            .collect(),
    })
}

pub(super) struct PluginCommandExecution {
    pub(super) status: i32,
    pub(super) outcome: PluginCommandOutcome,
}

pub(super) fn run_plugin_command_internal(
    plugin_id: &str,
    command_name: &str,
    args: &[String],
    kernel_client_factory: Option<&KernelClientFactory>,
) -> Result<PluginCommandExecution> {
    let state = runtime_command_state()?;
    let config = &state.config;
    let paths = &state.paths;
    let registry = &state.registry;
    let available = registry.plugin_ids();
    let plugin = registry
        .get(plugin_id)
        .with_context(|| format_plugin_not_found_message(plugin_id, &available))?;
    let enabled_plugins = state.enabled_plugins.clone();

    if !enabled_plugins.iter().any(|enabled| enabled == plugin_id) {
        anyhow::bail!(format_plugin_not_enabled_message(plugin_id));
    }

    let loaded = load_cached_plugin(plugin, &state)?;
    let plugin_search_roots = state.plugin_search_roots.clone();
    let available_capabilities = state
        .available_capability_providers
        .keys()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let context = plugin_command_context(
        config,
        paths,
        &plugin.declaration,
        command_name,
        args,
        available_service_descriptors(config, registry)?,
        available_capabilities,
        enabled_plugins,
        plugin_search_roots,
        state.registered_plugin_infos.clone(),
    );
    let _host_kernel_connection_guard = enter_host_kernel_connection(context.connection.clone());
    let _host_kernel_factory_guard =
        kernel_client_factory.map(|f| enter_host_kernel_client_factory(Arc::clone(f)));
    let run_result =
        loaded.run_command_with_context_and_outcome(command_name, args, Some(&context));
    let (status, outcome) = run_result.map_err(|error| {
        anyhow::anyhow!(format_plugin_command_run_error(
            plugin_id,
            command_name,
            &error
        ))
    })?;
    Ok(PluginCommandExecution { status, outcome })
}

fn load_cached_plugin(
    plugin: &bmux_plugin::RegisteredPlugin,
    state: &RuntimeCommandState,
) -> Result<Rc<bmux_plugin::LoadedPlugin>> {
    let plugin_id = plugin.declaration.id.as_str().to_string();
    LOADED_PLUGIN_CACHE.with(|slot| {
        if let Some(existing) = slot.borrow().get(&plugin_id) {
            return Ok(Rc::clone(existing));
        }

        let loaded = load_plugin(
            plugin,
            &plugin_host_metadata(),
            &state.available_capability_providers,
        )
        .with_context(|| format!("failed loading enabled plugin '{plugin_id}'"))?;
        let loaded = Rc::new(loaded);
        slot.borrow_mut().insert(plugin_id, Rc::clone(&loaded));
        Ok(loaded)
    })
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

    #[allow(clippy::wildcard_imports)]
    use super::*;
    use crate::runtime::attach::runtime::{
        plugin_fallback_new_context_id, plugin_fallback_retarget_context_id,
    };
    use crate::runtime::built_in_commands::BuiltInHandlerId;
    use crate::runtime::cli_parse::{ParsedRuntimeCli, parse_runtime_cli_with_registry};
    use crate::runtime::dispatch::built_in_handler_for_command;

    use crate::runtime::session_cli::format_destructive_op_error;
    use crate::runtime::terminal_doctor::{filter_trace_events, merged_runtime_keybindings};
    use crate::runtime::terminal_protocol::{ProtocolDirection, ProtocolTraceEvent};
    use bmux_cli_schema::{Command, TraceFamily};
    use bmux_client::ClientError;
    use bmux_config::{BmuxConfig, ConfigPaths};
    use bmux_ipc::ErrorCode;
    use bmux_plugin::{PluginManifest, PluginRegistry};
    use std::ffi::OsString;
    use std::fs;
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

        assert!(validate_enabled_plugins(&config, &registry).is_ok());
    }

    #[test]
    fn validate_enabled_plugins_rejects_when_required_namespace_is_unowned() {
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
                    "owns_namespaces=['plugin']\n[[commands]]\nname='list'\npath=['plugin','list']\nsummary='list'\nexecution='provider_exec'\nexpose_in_cli=true\n",
                ),
            )
            .expect("plugin should register");

        let mut config = BmuxConfig::default();
        config.plugins.disabled.push("bmux.plugin_cli".to_string());
        config
            .plugins
            .routing
            .required_namespaces
            .push(bmux_config::RequiredNamespaceClaim {
                namespace: "plugin".to_string(),
                owner: None,
            });

        let error = validate_enabled_plugins(&config, &registry)
            .expect_err("required namespace claim should fail when unowned");
        assert!(
            error
                .to_string()
                .contains("required namespace claim 'plugin' is not owned")
        );
    }

    #[test]
    fn validate_enabled_plugins_rejects_when_required_namespace_owner_mismatches() {
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
                    "owns_namespaces=['plugin']\n[[commands]]\nname='list'\npath=['plugin','list']\nsummary='list'\nexecution='provider_exec'\nexpose_in_cli=true\n",
                ),
            )
            .expect("plugin should register");

        let mut config = BmuxConfig::default();
        config
            .plugins
            .routing
            .required_namespaces
            .push(bmux_config::RequiredNamespaceClaim {
                namespace: "plugin".to_string(),
                owner: Some("third.party".to_string()),
            });

        let error = validate_enabled_plugins(&config, &registry)
            .expect_err("owner mismatch should fail startup validation");
        assert!(
            error
                .to_string()
                .contains("required namespace claim 'plugin'")
        );
    }

    #[test]
    fn effective_enabled_plugins_includes_bundled_plugins_by_default() {
        let Some(bundled_root) = bundled_plugin_root() else {
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
        let enabled = effective_enabled_plugins(&config, &registry);
        assert!(enabled.iter().any(|plugin_id| plugin_id == "bmux.windows"));
    }

    #[test]
    fn effective_enabled_plugins_include_windows_and_permissions_by_default() {
        let Some(bundled_root) = bundled_plugin_root() else {
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
        let enabled = effective_enabled_plugins(&config, &registry);
        assert!(enabled.iter().any(|plugin_id| plugin_id == "bmux.windows"));
        assert!(
            enabled
                .iter()
                .any(|plugin_id| plugin_id == "bmux.permissions")
        );
    }

    #[test]
    fn effective_enabled_plugins_honors_disabled_overrides() {
        let Some(bundled_root) = bundled_plugin_root() else {
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
        let enabled = effective_enabled_plugins(&config, &registry);
        assert!(!enabled.iter().any(|plugin_id| plugin_id == "bmux.windows"));
    }

    #[test]
    fn effective_enabled_plugins_skips_bundled_plugins_with_missing_entry() {
        let Some(bundled_root) = bundled_plugin_root() else {
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
        let enabled = effective_enabled_plugins(&config, &registry);
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

        assert!(validate_enabled_plugins(&config, &registry).is_ok());
    }

    #[test]
    fn validate_enabled_plugins_rejects_missing_plugin() {
        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("missing.plugin".to_string());

        let error = validate_enabled_plugins(&config, &PluginRegistry::new())
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

        assert!(validate_configured_plugins(&config, &paths).is_ok());
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
                        "owns_namespaces=['roles','session']\n[[commands]]\nname='roles'\npath=['roles']\naliases=[[\"session\",\"roles\"]]\nsummary='list'\nexecution='provider_exec'\nexpose_in_cli=true\n[[commands.arguments]]\nname='session'\nkind='string'\nlong='session'\nrequired=true\n",
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

        let parsed = parse_runtime_cli_with_registry(
            &argv,
            &config,
            &registry,
            bmux_config::ConfigLoadOverrides::default(),
        )
        .expect("runtime CLI should parse plugin alias under session namespace");
        match parsed {
            ParsedRuntimeCli::Plugin {
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
                        "owns_namespaces=['plugin']\n[[commands]]\nname='list'\npath=['plugin','list']\nsummary='list'\nexecution='provider_exec'\nexpose_in_cli=true\n",
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

        let parsed = parse_runtime_cli_with_registry(
            &argv,
            &config,
            &registry,
            bmux_config::ConfigLoadOverrides::default(),
        )
        .expect("runtime CLI should parse plugin-owned plugin namespace command");
        match parsed {
            ParsedRuntimeCli::Plugin {
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
        let Some(bundled_root) = bundled_plugin_root() else {
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
                        "owns_namespaces=['new-window']\n[[commands]]\nname='new-window'\npath=['new-window']\nsummary='new'\nexecution='provider_exec'\nexpose_in_cli=true\n",
                    ),
                )
                .expect("plugin should register");

        let config = BmuxConfig::default();
        let argv = vec![OsString::from("bmux"), OsString::from("new-window")];
        let parsed = parse_runtime_cli_with_registry(
            &argv,
            &config,
            &registry,
            bmux_config::ConfigLoadOverrides::default(),
        )
        .expect("runtime CLI should parse bundled plugin command");
        match parsed {
            ParsedRuntimeCli::Plugin { plugin_id, .. } => {
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

        let parsed = parse_runtime_cli_with_registry(
            &argv,
            &config,
            &registry,
            bmux_config::ConfigLoadOverrides::default(),
        )
        .expect("runtime CLI should parse built-in attach command");

        match parsed {
            ParsedRuntimeCli::BuiltIn { cli, .. } => {
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
    #[allow(clippy::too_many_lines)]
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
            execution_class: bmux_plugin::PluginExecutionClass::NativeStandard,
            owns_namespaces: std::collections::BTreeSet::new(),
            owns_paths: std::collections::BTreeSet::new(),
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
            ready_signals: Vec::new(),
        };
        let context = plugin_lifecycle_context(
            &config,
            &paths,
            &declaration,
            service_descriptors_from_declarations([&declaration]),
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
                .any(|service| service.interface_id == "cli-command/v1")
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
                .any(|service| service.interface_id == "provider-command/v1")
        );
        assert_eq!(
            context.settings.as_ref().and_then(|value| value.as_str()),
            Some("configured")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
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
            execution_class: bmux_plugin::PluginExecutionClass::NativeStandard,
            owns_namespaces: std::collections::BTreeSet::new(),
            owns_paths: std::collections::BTreeSet::new(),
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
            ready_signals: Vec::new(),
        };

        let context = plugin_command_context(
            &config,
            &paths,
            &declaration,
            "run-action",
            &["--name".to_string(), "editor".to_string()],
            service_descriptors_from_declarations([&declaration]),
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
                .any(|service| service.interface_id == "cli-command/v1")
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
    }

    #[test]
    fn plugin_system_event_uses_system_kind_and_name() {
        let event = plugin_system_event("server_started");
        assert_eq!(event.kind.as_str(), "bmux.core/server_started");
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
        let event = plugin_event_from_server_event(&bmux_client::ServerEvent::SessionCreated {
            id: session_id,
            name: Some("editor".to_string()),
        })
        .expect("plugin event should build");
        let session_id_text = session_id.to_string();
        assert_eq!(event.kind.as_str(), "bmux.core/session_created");
        assert!(event.payload.to_string().contains(&session_id_text));
    }

    #[test]
    fn built_in_handler_mapping_stays_in_sync_for_core_native_commands() {
        let command = Command::KillSession {
            target: "dev".to_string(),
            force_local: false,
        };
        assert_eq!(
            built_in_handler_for_command(&command),
            BuiltInHandlerId::KillSession
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
        let message = format_destructive_op_error(
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
        let message = format_destructive_op_error(
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
        let message = format_plugin_command_run_error("bmux.windows", "kill", &error);
        assert!(message.contains("failed running plugin command 'bmux.windows:kill'"));
        assert!(message.contains("operation denied by an active policy provider"));
        assert!(message.contains("authorized principal"));
    }

    #[test]
    fn format_plugin_command_run_error_keeps_generic_failures_without_hint() {
        let error = anyhow::anyhow!("unsupported service operation");
        let message = format_plugin_command_run_error("bmux.permissions", "grant", &error);
        assert!(message.contains("failed running plugin command 'bmux.permissions:grant'"));
        assert!(!message.contains("operation denied by session policy"));
    }

    #[test]
    fn unknown_external_command_message_points_to_plugin_list_help() {
        let message =
            unknown_external_command_message(&["session".to_string(), "roles".to_string()]);
        assert!(message.contains("unknown command 'session roles'"));
        assert!(message.contains("bmux plugin list"));
    }

    #[test]
    fn format_plugin_not_found_message_lists_available_plugins() {
        let message = format_plugin_not_found_message(
            "missing.plugin",
            &["bmux.windows".to_string(), "bmux.permissions".to_string()],
        );
        assert!(message.contains("plugin 'missing.plugin' was not found"));
        assert!(message.contains("bmux.windows, bmux.permissions"));
    }

    #[test]
    fn format_plugin_not_found_message_handles_empty_registry() {
        let empty: [&str; 0] = [];
        let message = format_plugin_not_found_message("missing.plugin", &empty);
        assert_eq!(message, "plugin 'missing.plugin' was not found");
    }

    #[test]
    fn format_plugin_not_enabled_message_points_to_plugins_enabled() {
        let message = format_plugin_not_enabled_message("bmux.windows");
        assert!(message.contains("plugin 'bmux.windows' is not enabled"));
        assert!(message.contains("plugins.disabled"));
        assert!(message.contains("plugins.enabled"));
    }

    #[test]
    fn format_plugin_argument_validation_error_adds_help_hint_for_missing_required() {
        let error = anyhow::anyhow!("missing required option '--session'");
        let message = format_plugin_argument_validation_error(
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
        let message = format_plugin_argument_validation_error(
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
            plugin_fallback_retarget_context_id(before, after, attached, false),
            after
        );
    }

    #[test]
    fn plugin_fallback_retarget_context_id_ignores_when_outcome_already_applied() {
        let before = Some(Uuid::from_u128(1));
        let after = Some(Uuid::from_u128(2));
        let attached = Some(Uuid::from_u128(2));

        assert_eq!(
            plugin_fallback_retarget_context_id(before, after, attached, true),
            None
        );
    }

    #[test]
    #[allow(clippy::iter_on_single_items)]
    fn plugin_fallback_new_context_id_returns_single_new_context() {
        let before = [Uuid::from_u128(1)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let after = [Uuid::from_u128(1), Uuid::from_u128(2)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            plugin_fallback_new_context_id(
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
    #[allow(clippy::iter_on_single_items)]
    fn plugin_fallback_new_context_id_prefers_after_context_when_multiple_new() {
        let before = [Uuid::from_u128(1)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let after = [Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            plugin_fallback_new_context_id(
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
    #[allow(clippy::iter_on_single_items)]
    fn plugin_fallback_new_context_id_ignores_when_outcome_applied() {
        let before = [Uuid::from_u128(1)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let after = [Uuid::from_u128(1), Uuid::from_u128(2)]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            plugin_fallback_new_context_id(
                Some(&before),
                Some(&after),
                Some(Uuid::from_u128(1)),
                Some(Uuid::from_u128(2)),
                true,
            ),
            None
        );
    }
}
