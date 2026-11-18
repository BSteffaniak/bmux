mod kernel_policy;
mod owned_frontends;
pub(super) use owned_frontends::OwnedFrontends;
#[cfg(feature = "bundled-plugin-recording")]
pub(super) mod recording_binding;
mod selected;
pub(super) use selected::ProductionPlugin;

use anyhow::{Context, Result};
use bmux_config::{BmuxConfig, ConfigPaths, PluginSourceConfig};
use bmux_plugin::{PluginManifest, PluginRegistry, PluginRuntime, load_process_plugin};
use bmux_plugin_sdk::{
    CURRENT_PLUGIN_ABI_VERSION, CURRENT_PLUGIN_API_VERSION, HostConnectionInfo, HostMetadata,
    HostScope, NativeCommandContext, NativeLifecycleContext, PluginCommandOutcome, PluginEvent,
    PluginEventKind, RegisteredService,
    perf_telemetry::{PhaseChannel, PhasePayload, emit as emit_phase_timing},
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, instrument, warn};

use super::{
    KernelClientFactory, available_capability_providers, available_service_descriptors,
    connect_raw, core_provided_capabilities, enter_host_kernel_client_factory,
    enter_host_kernel_connection, host_kernel_bridge, plugin_commands::PluginCommandRegistry,
    plugin_host, server_event_name, service_descriptors_from_declarations,
};

/// One explicitly resolved command runtime. Its runtime owner is supplied by
/// whoever built it, never rediscovered from task-local or thread context.
#[derive(Debug, Clone)]
pub struct RuntimeCommandState {
    pub(super) runtime_owner: Option<crate::runtime_owner::WeakRuntimeOwner>,
    pub(super) config: BmuxConfig,
    pub(super) paths: ConfigPaths,
    pub(super) registry: Arc<PluginRegistry>,
    pub(super) enabled_plugins: Vec<String>,
    pub(super) available_capability_providers: BTreeMap<HostScope, bmux_plugin::CapabilityProvider>,
    pub(super) available_capability_names: Vec<String>,
    pub(super) available_services: Vec<RegisteredService>,
    pub(super) plugin_search_roots: Vec<String>,
    pub(super) registered_plugin_infos: Vec<bmux_plugin_sdk::RegisteredPluginInfo>,
}

thread_local! {
    static RUNTIME_COMMAND_STATE_CACHE: RefCell<Option<RuntimeCommandState>> = const { RefCell::new(None) };
    static LOADED_PLUGIN_CACHE: RefCell<BTreeMap<String, Rc<ProductionPlugin>>> = const { RefCell::new(BTreeMap::new()) };
}

// One plugin runtime scope, loader and host state per process, never per OS
// thread: nested plugin commands run on callback threads, and a thread-local
// scope made their instances a "different runtime" from their parent entry,
// so child callback binding was rejected.
static EXECUTION_LOADER: std::sync::LazyLock<bmux_plugin::NativePluginLoader> =
    std::sync::LazyLock::new(bmux_plugin::NativePluginLoader::new);
static EXECUTION_SCOPE: std::sync::LazyLock<bmux_plugin_runtime::PluginRuntimeScopeId> =
    std::sync::LazyLock::new(bmux_plugin_runtime::PluginRuntimeScopeId::new);
static HOST_STATE: std::sync::LazyLock<Arc<bmux_plugin::PluginStateRegistry>> =
    std::sync::LazyLock::new(|| Arc::new(bmux_plugin::PluginStateRegistry::new()));

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
/// Declares bundled implementations once for inert shared-definition registration
/// and bundled client-adapter discovery. Every entry names the plugin-owned
/// `plugin_definition()` that generated dynamic packaging also uses, so bundled
/// and dynamic builds share one implementation. Each declaration remains
/// feature-gated for no-plugin builds.
macro_rules! declare_bundled_plugins {
    ($(
        feature = $feature:literal,
        id = $plugin_id:literal,
        definition = $definition:expr
        $(, kernel_policy = $kernel_policy:expr)?;
    )*) => {
        #[cfg(test)]
        #[allow(unused_variables, clippy::missing_const_for_fn)]
        fn register_static_bundled_plugins(registry: &mut PluginRegistry) {
            register_selected_bundled_plugins(registry, &BTreeMap::new())
                .expect("bundled declarations must register");
        }

        #[allow(unused_variables, clippy::missing_const_for_fn)]
        fn register_selected_bundled_plugins(
            registry: &mut PluginRegistry,
            sources: &BTreeMap<String, PluginSourceConfig>,
        ) -> Result<()> {
            $(
                #[cfg(feature = $feature)]
                if !matches!(sources.get($plugin_id), Some(PluginSourceConfig::Dynamic { .. })) {
                    let manifest = register_bundled_definition($plugin_id, $definition)?;
                    registry.register_bundled_manifest(&manifest)
                        .with_context(|| format!("failed registering bundled plugin '{}'", $plugin_id))?;
                }
            )*
            Ok(())
        }

        #[allow(unused_variables, clippy::missing_const_for_fn)]
        fn trusted_bundled_kernel_policy(
            plugin: &bmux_plugin::RegisteredPlugin,
        ) -> bmux_plugin_sdk::Result<Option<kernel_policy::TrustedKernelPolicy>> {
            if !plugin.bundled_static {
                // A native override never inherits policy from its logical ID.
                // Explicit selected-artifact approval requires a separate entry.
                return Ok(None);
            }
            $(
                $(
                    #[cfg(feature = $feature)]
                    if plugin.manifest.id == $plugin_id {
                        let artifact = bmux_plugin::NativePluginLoader::registered_definition($plugin_id)?
                            .ok_or_else(|| bmux_plugin_sdk::PluginError::ServiceProtocol {
                                details: "trusted composition definition unavailable".into(),
                            })?;
                        return kernel_policy::TrustedKernelPolicy::bundled(artifact.metadata(), $kernel_policy).map(Some);
                    }
                )?
            )*
            Ok(None)
        }

        #[allow(unused_variables, clippy::missing_const_for_fn)]
        fn install_static_bundled_client_adapters(
            enabled: &[String],
            paths: &ConfigPaths,
        ) {
            $(
                #[cfg(feature = $feature)]
                if enabled.iter().any(|plugin_id| plugin_id == $plugin_id) {
                    install_bundled_client_adapter($plugin_id, paths);
                }
            )*
        }

    };
}

fn register_bundled_definition(
    plugin_id: &str,
    definition: bmux_plugin_sdk::definition::PluginDefinition,
) -> Result<String> {
    anyhow::ensure!(
        definition.metadata.plugin_id == plugin_id,
        "bundled definition identity differs from declaration '{plugin_id}'"
    );
    let embedded = definition
        .metadata
        .manifest_toml
        .as_deref()
        .context("bundled definition has no embedded manifest")?;
    // The embedded manifest is generated from the plugin's BPDL world; the
    // application registers it verbatim and never repairs contract metadata.
    let embedded = embedded.to_owned();
    if let Some(existing) = bmux_plugin::NativePluginLoader::registered_definition(plugin_id)? {
        anyhow::ensure!(
            existing.metadata() == &definition.metadata,
            "conflicting bundled definition for '{plugin_id}'"
        );
    } else {
        bmux_plugin::NativePluginLoader::register_definition(definition)?;
    }
    Ok(embedded)
}

/// The attach companions bundled plugins contributed to this CLI process.
/// Host-private: attach runtimes start them with their own resources.
pub(in crate::runtime) fn host_attach_companions() -> &'static bmux_plugin::AttachCompanionRegistry
{
    static COMPANIONS: std::sync::OnceLock<bmux_plugin::AttachCompanionRegistry> =
        std::sync::OnceLock::new();
    COMPANIONS.get_or_init(bmux_plugin::AttachCompanionRegistry::new)
}

/// Register an optional client-side adapter owned by one bundled plugin.
#[cfg(any(
    feature = "bundled-plugin-connections",
    feature = "bundled-plugin-cluster",
    feature = "bundled-plugin-tab-bar",
    feature = "bundled-plugin-sidebar",
    feature = "bundled-plugin-pane-runtime"
))]
#[allow(unused_variables, clippy::missing_const_for_fn)]
fn install_bundled_client_adapter(plugin_id: &str, paths: &ConfigPaths) {
    #[cfg(feature = "bundled-plugin-connections")]
    if plugin_id == "bmux.connections" {
        crate::runtime::attach::provider::register_plugin_provider(
            bmux_connections_plugin::client_attach_provider(paths),
        );
    }
    #[cfg(feature = "bundled-plugin-pane-runtime")]
    if plugin_id == "bmux.pane_runtime" {
        crate::runtime::attach::provider::register_plugin_provider(
            bmux_pane_runtime_plugin::observation::provider(),
        );
    }
    #[cfg(feature = "bundled-plugin-broadcast")]
    if plugin_id == "bmux.broadcast" {
        let paths = paths.clone();
        host_attach_companions().register(bmux_plugin::AttachCompanion::from_factory(
            plugin_id,
            std::sync::Arc::new(move |resources| {
                Ok(Box::new(
                    bmux_broadcast_plugin::BroadcastPresentation::install(
                        paths.clone(),
                        resources.states.clone(),
                    )?,
                ))
            }),
        ));
    }
    #[cfg(feature = "bundled-plugin-cluster")]
    if plugin_id == "bmux.cluster" {
        crate::runtime::attach::provider::register_plugin_provider(
            bmux_cluster_plugin_client::attach_provider(),
        );
    }
    #[cfg(feature = "bundled-plugin-tab-bar")]
    if plugin_id == "bmux.tab_bar" {
        // Settings come from the invoking attach runtime's config, not the
        // config this process registered companions with.
        host_attach_companions().register(bmux_plugin::AttachCompanion::from_factory(
            plugin_id,
            std::sync::Arc::new(|resources| {
                Ok(Box::new(bmux_tab_bar_plugin::TabBarPresentation::install(
                    resources.settings_for("bmux.tab_bar"),
                    resources,
                )?))
            }),
        ));
    }
    #[cfg(feature = "bundled-plugin-sidebar")]
    if plugin_id == "bmux.sidebar" {
        host_attach_companions().register(bmux_plugin::AttachCompanion::from_factory(
            plugin_id,
            std::sync::Arc::new(|resources| {
                let presentation = bmux_sidebar_plugin::SidebarPresentation::install(
                    resources.settings_for("bmux.sidebar"),
                    resources.layouts.clone(),
                    resources.allocations.clone(),
                    resources.input.clone(),
                    resources.surfaces.clone(),
                )?;
                presentation.start(&resources.events)?;
                Ok(Box::new(presentation))
            }),
        ));
    }
}

declare_bundled_plugins! {
    feature = "bundled-plugin-clients",
    id = "bmux.clients",
    definition = bmux_clients_plugin::plugin_definition(),
    kernel_policy = bmux_clients_plugin::authorize_kernel_request;

    feature = "bundled-plugin-clipboard",
    id = "bmux.clipboard",
    definition = bmux_clipboard_plugin::plugin_definition();

    feature = "bundled-plugin-cluster",
    id = "bmux.cluster",
    definition = bmux_cluster_plugin::plugin_definition();

    feature = "bundled-plugin-connections",
    id = "bmux.connections",
    definition = bmux_connections_plugin::plugin_definition();

    feature = "bundled-plugin-command-palette",
    id = "bmux.command_palette",
    definition = bmux_command_palette_plugin::plugin_definition();

    feature = "bundled-plugin-contexts",
    id = "bmux.contexts",
    definition = bmux_contexts_plugin::plugin_definition();

    feature = "bundled-plugin-control-catalog",
    id = "bmux.control_catalog",
    definition = bmux_control_catalog_plugin::plugin_definition();

    feature = "bundled-plugin-performance",
    id = "bmux.performance",
    definition = bmux_performance_plugin::plugin_definition();

    feature = "bundled-plugin-permissions",
    id = "bmux.permissions",
    definition = bmux_permissions_plugin::plugin_definition();

    feature = "bundled-plugin-cli",
    id = "bmux.plugin_cli",
    definition = bmux_plugin_cli_plugin::plugin_definition();

    feature = "bundled-plugin-prompted-actions",
    id = "bmux.prompted_actions",
    definition = bmux_prompted_actions_plugin::plugin_definition();

    feature = "bundled-plugin-recording",
    id = "bmux.recording",
    definition = recording_binding::unbound_definition();

    feature = "bundled-plugin-sessions",
    id = "bmux.sessions",
    definition = bmux_sessions_plugin::plugin_definition();

    feature = "bundled-plugin-playbook",
    id = "bmux.playbook",
    definition = bmux_playbook_plugin::plugin_definition();

    feature = "bundled-plugin-broadcast",
    id = "bmux.broadcast",
    definition = bmux_broadcast_plugin::plugin_definition();

    feature = "bundled-plugin-presenter",
    id = "bmux.presenter",
    definition = bmux_presenter_plugin::plugin_definition();

    feature = "bundled-plugin-finder",
    id = "bmux.finder",
    definition = bmux_finder_plugin::plugin_definition();

    feature = "bundled-plugin-pane-runtime",
    id = "bmux.pane_runtime",
    definition = bmux_pane_runtime_plugin::plugin_definition();

    feature = "bundled-plugin-application-resurrection",
    id = "bmux.application_resurrection",
    definition = bmux_application_resurrection_plugin::plugin_definition();

    feature = "bundled-plugin-snapshot",
    id = "bmux.snapshot",
    definition = bmux_snapshot_plugin::plugin_definition();

    feature = "bundled-plugin-sidebar",
    id = "bmux.sidebar",
    definition = bmux_sidebar_plugin::plugin_definition();

    feature = "bundled-plugin-tab-bar",
    id = "bmux.tab_bar",
    definition = bmux_tab_bar_plugin::plugin_definition();

    feature = "bundled-plugin-theme",
    id = "bmux.theme",
    definition = bmux_theme_plugin::plugin_definition();

    feature = "bundled-plugin-windows",
    id = "bmux.tabs",
    definition = bmux_tabs_plugin::plugin_definition();

    feature = "bundled-plugin-workspaces",
    id = "bmux.workspaces",
    definition = bmux_workspaces_plugin::plugin_definition();

    feature = "bundled-plugin-decoration",
    id = "bmux.decoration",
    definition = bmux_decoration_plugin::plugin_definition();
}

/// Load one selected implementation into this host's scoped executor.
#[allow(clippy::result_large_err)] // Plugin error types carry context payloads
pub(super) fn load_plugin(
    plugin: &bmux_plugin::RegisteredPlugin,
    host: &HostMetadata,
    available_capabilities: &std::collections::BTreeMap<HostScope, bmux_plugin::CapabilityProvider>,
    config: &BmuxConfig,
) -> bmux_plugin_sdk::Result<ProductionPlugin> {
    let state = selected_command_state().map_err(|error| {
        bmux_plugin_sdk::PluginError::ServiceProtocol {
            details: format!("selected loader requires resolved runtime paths: {error:#}"),
        }
    })?;
    load_plugin_with_definitions(
        plugin,
        host,
        available_capabilities,
        config,
        &BTreeMap::new(),
        &state,
        None,
    )
}

#[allow(clippy::result_large_err)] // Preserve the loader's public error contract.
fn load_plugin_with_definitions(
    plugin: &bmux_plugin::RegisteredPlugin,
    host: &HostMetadata,
    available_capabilities: &BTreeMap<HostScope, bmux_plugin::CapabilityProvider>,
    config: &BmuxConfig,
    definitions: &BTreeMap<String, bmux_plugin_sdk::definition::PluginDefinition>,
    state: &RuntimeCommandState,
    cli_command: Option<&str>,
) -> bmux_plugin_sdk::Result<ProductionPlugin> {
    let paths = &state.paths;
    let approvals = kernel_policy::load_native_approvals(paths)?;
    ProductionPlugin::load(
        plugin,
        host,
        available_capabilities,
        config,
        selected::ExecutionOwner {
            cli_command,
            runtime_owner: state
                .runtime_owner
                .as_ref()
                .and_then(crate::runtime_owner::WeakRuntimeOwner::upgrade),
            runtime: *EXECUTION_SCOPE,
            host_state: Arc::clone(&HOST_STATE),
            loader: &EXECUTION_LOADER,
            definitions,
            approval_store: approvals.as_ref(),
            runtime_dir: &paths.runtime_dir,
        },
    )
}

// Keep discovery, source precedence and its timing receipt in one auditable pass.
#[allow(clippy::too_many_lines)]
pub fn scan_available_plugins(config: &BmuxConfig, paths: &ConfigPaths) -> Result<PluginRegistry> {
    let total_started = Instant::now();
    let workspace_bundled_root = workspace_bundled_plugin_root();
    let search_paths = resolve_plugin_search_paths(config, paths)?;
    let search_root_count = search_paths.len();
    let discover_started = Instant::now();
    let reports = bmux_plugin::discover_plugin_manifests_in_roots(&search_paths)?;
    let discover_us = discover_started.elapsed().as_micros();
    let manifest_count = reports
        .iter()
        .map(|report| report.manifest_paths.len())
        .sum::<usize>();
    let mut registry = PluginRegistry::new();

    let static_started = Instant::now();
    // Register statically-linked bundled plugins first (behind feature flags).
    register_selected_bundled_plugins(&mut registry, &config.plugins.sources)?;
    // Resolve explicit selections before ambient discovery. Neither duplicate
    // search results nor workspace artifacts may replace a selected source.
    register_selected_dynamic_plugins(&mut registry, config)?;
    let static_register_us = static_started.elapsed().as_micros();
    let static_bundled_count = registry
        .iter()
        .filter(|plugin| plugin.bundled_static)
        .count();

    let filesystem_started = Instant::now();
    let mut filesystem_registered_count = 0_usize;
    let mut inventory = bmux_plugin::PluginCandidateInventory::new();
    for plugin in registry.iter() {
        inventory.add_registered_plugin(plugin)?;
    }
    for report in reports {
        for manifest_path in report.manifest_paths {
            if workspace_bundled_root
                .as_ref()
                .is_some_and(|root| report.search_root == *root)
                && static_bundled_workspace_manifest_is_registered(&manifest_path)
            {
                continue;
            }
            match PluginManifest::from_path(&manifest_path) {
                Ok(mut manifest) => {
                    if config.plugins.sources.contains_key(&manifest.id) {
                        continue;
                    }
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
                    inventory
                        .add_dynamic_manifest(&report.search_root, &manifest_path, manifest)
                        .with_context(|| {
                            format!("failed inventorying plugin {}", manifest_path.display())
                        })?;
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
    // Default preference chooses bundled when present; otherwise more than one
    // dynamic artifact is an explicit ambiguity, never directory-order policy.
    let candidate_ids = inventory
        .iter()
        .map(|candidate| candidate.plugin.manifest.id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    for plugin_id in candidate_ids {
        if registry.get(&plugin_id).is_some() {
            continue;
        }
        let selected = inventory
            .select(&bmux_plugin::PluginSelectionRequest {
                plugin_id: plugin_id.clone(),
                role: inventory_component_role(
                    &inventory,
                    &plugin_id,
                    &bmux_plugin::PluginCandidatePreference::Default,
                )?,
                preference: bmux_plugin::PluginCandidatePreference::Default,
                plugin_api: CURRENT_PLUGIN_API_VERSION,
                native_abi: CURRENT_PLUGIN_ABI_VERSION,
                contracts: Vec::new(),
            })
            .with_context(|| format!("failed selecting plugin '{plugin_id}'"))?;
        registry.register_selected_candidate(&selected)?;
        filesystem_registered_count += 1;
    }
    let filesystem_register_us = filesystem_started.elapsed().as_micros();
    emit_plugin_runtime_phase_timing(
        &PhasePayload::new("plugin.registry_scan")
            .field("search_root_count", search_root_count)
            .field("manifest_count", manifest_count)
            .field("static_bundled_count", static_bundled_count)
            .field("filesystem_registered_count", filesystem_registered_count)
            .field("discover_us", discover_us)
            .field("static_register_us", static_register_us)
            .field("filesystem_register_us", filesystem_register_us)
            .field("total_us", total_started.elapsed().as_micros())
            .finish(),
    );
    Ok(registry)
}

/// Resolve explicit sources without modifying ambient discovery precedence.
/// Errors are fatal: an unavailable selected artifact is never a fallback signal.
fn register_selected_dynamic_plugins(
    registry: &mut PluginRegistry,
    config: &BmuxConfig,
) -> Result<()> {
    // Reuse the host inventory's canonical locator, identity, compatibility and
    // role checks; source selection must not acquire a second CLI implementation.
    let mut inventory = bmux_plugin::PluginCandidateInventory::new();
    for plugin in registry.iter() {
        inventory.add_registered_plugin(plugin)?;
    }
    for (plugin_id, source) in &config.plugins.sources {
        let preference = match source {
            PluginSourceConfig::Bundled => bmux_plugin::PluginCandidatePreference::Bundled,
            PluginSourceConfig::Dynamic { manifest } => {
                inventory
                    .add_dynamic_manifest_path_for_plugin(plugin_id, manifest)
                    .with_context(|| {
                        format!("failed inventorying selected source for plugin '{plugin_id}'")
                    })?;
                bmux_plugin::PluginCandidatePreference::Dynamic {
                    manifest_path: manifest.clone(),
                }
            }
        };
        let selected = inventory
            .select(&bmux_plugin::PluginSelectionRequest {
                plugin_id: plugin_id.clone(),
                role: inventory_component_role(&inventory, plugin_id, &preference)?,
                preference,
                plugin_api: CURRENT_PLUGIN_API_VERSION,
                native_abi: CURRENT_PLUGIN_ABI_VERSION,
                contracts: Vec::new(),
            })
            .with_context(|| format!("failed selecting source for plugin '{plugin_id}'"))?;
        if matches!(source, PluginSourceConfig::Dynamic { .. }) {
            let plugin = &selected.candidate().plugin;
            anyhow::ensure!(
                plugin.manifest.runtime == PluginRuntime::Native,
                "selected dynamic plugin '{plugin_id}' must declare native runtime"
            );
            anyhow::ensure!(
                registered_plugin_entry_exists(plugin),
                "selected dynamic plugin '{plugin_id}' entry is not a file or is missing"
            );
            registry.register_selected_candidate(&selected)?;
        }
    }
    Ok(())
}

/// Select the deployed component before compatibility negotiation. Dual-role
/// declarations explicitly provide a backend; frontend-only declarations must
/// execute as frontend, never by retrying a failed backend selection.
fn deployed_component_role(
    manifest: &PluginManifest,
) -> std::result::Result<
    bmux_plugin_sdk::definition::ComponentRole,
    Box<bmux_plugin_sdk::PluginError>,
> {
    use bmux_plugin_sdk::definition::ComponentRole;
    match manifest.component_roles.as_slice() {
        [ComponentRole::Frontend] => Ok(ComponentRole::Frontend),
        roles if roles.contains(&ComponentRole::Backend) => Ok(ComponentRole::Backend),
        _ => Err(Box::new(bmux_plugin_sdk::PluginError::ServiceProtocol {
            details: format!(
                "plugin '{}' has no unambiguous deployed component role",
                manifest.id
            ),
        })),
    }
}

fn inventory_component_role(
    inventory: &bmux_plugin::PluginCandidateInventory,
    plugin_id: &str,
    preference: &bmux_plugin::PluginCandidatePreference,
) -> Result<bmux_plugin_sdk::definition::ComponentRole> {
    use bmux_plugin::{PluginCandidatePreference, PluginCandidateSource};
    let candidates = inventory
        .iter()
        .filter(|candidate| candidate.plugin.manifest.id == plugin_id)
        .collect::<Vec<_>>();
    let bundled = candidates
        .iter()
        .any(|candidate| candidate.source == PluginCandidateSource::Bundled);
    let override_path = match preference {
        PluginCandidatePreference::Dynamic { manifest_path } => {
            Some(std::fs::canonicalize(manifest_path)?)
        }
        PluginCandidatePreference::Default | PluginCandidatePreference::Bundled => None,
    };
    let mut eligible = candidates.into_iter().filter(|candidate| match preference {
        PluginCandidatePreference::Default => !bundled || candidate.source == PluginCandidateSource::Bundled,
        PluginCandidatePreference::Bundled => candidate.source == PluginCandidateSource::Bundled,
        PluginCandidatePreference::Dynamic { .. } => matches!(&candidate.source,
            PluginCandidateSource::Dynamic { manifest_path } if Some(manifest_path) == override_path.as_ref()),
    });
    let candidate = eligible
        .next()
        .with_context(|| format!("no candidates for '{plugin_id}'"))?;
    anyhow::ensure!(
        eligible.next().is_none(),
        "ambiguous candidates for '{plugin_id}'; select an explicit manifest"
    );
    Ok(deployed_component_role(&candidate.plugin.manifest)?)
}

fn static_bundled_workspace_manifest_is_registered(manifest_path: &Path) -> bool {
    if manifest_path.file_name().and_then(|name| name.to_str()) != Some("plugin.toml") {
        return false;
    }
    let Some(dir_name) = manifest_path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    static_bundled_workspace_plugin_dirs().contains(&dir_name)
}

const fn static_bundled_workspace_plugin_dirs() -> &'static [&'static str] {
    &[
        #[cfg(feature = "bundled-plugin-clients")]
        "clients-plugin",
        #[cfg(feature = "bundled-plugin-clipboard")]
        "clipboard-plugin",
        #[cfg(feature = "bundled-plugin-cluster")]
        "cluster-plugin",
        #[cfg(feature = "bundled-plugin-connections")]
        "connections-plugin",
        #[cfg(feature = "bundled-plugin-command-palette")]
        "command-palette-plugin",
        #[cfg(feature = "bundled-plugin-contexts")]
        "contexts-plugin",
        #[cfg(feature = "bundled-plugin-control-catalog")]
        "control-catalog-plugin",
        #[cfg(feature = "bundled-plugin-finder")]
        "finder-plugin",
        #[cfg(feature = "bundled-plugin-performance")]
        "performance-plugin",
        #[cfg(feature = "bundled-plugin-permissions")]
        "permissions-plugin",
        #[cfg(feature = "bundled-plugin-cli")]
        "plugin-cli-plugin",
        #[cfg(feature = "bundled-plugin-prompted-actions")]
        "prompted-actions-plugin",
        #[cfg(feature = "bundled-plugin-recording")]
        "recording-plugin",
        #[cfg(feature = "bundled-plugin-sessions")]
        "sessions-plugin",
        #[cfg(feature = "bundled-plugin-pane-runtime")]
        "pane-runtime-plugin",
        #[cfg(feature = "bundled-plugin-application-resurrection")]
        "application-resurrection-plugin",
        #[cfg(feature = "bundled-plugin-snapshot")]
        "snapshot-plugin",
        #[cfg(feature = "bundled-plugin-theme")]
        "theme-plugin",
        #[cfg(feature = "bundled-plugin-windows")]
        "tabs-plugin",
        #[cfg(feature = "bundled-plugin-workspaces")]
        "workspaces-plugin",
        #[cfg(feature = "bundled-plugin-decoration")]
        "decoration-plugin",
    ]
}

pub(super) fn initialize_native_runtime_state(
    config: BmuxConfig,
    paths: ConfigPaths,
) -> Result<()> {
    let registry = Arc::new(scan_available_plugins(&config, &paths)?);
    install_enabled_client_adapters(&config, &registry, &paths);
    // Embedding hosts do not hand over a primary runtime owner, so selected
    // plugin execution from this state fails explicitly instead of guessing one.
    let state = build_runtime_command_state(config, paths, registry, None)?;
    RUNTIME_COMMAND_STATE_CACHE.with(|slot| *slot.borrow_mut() = Some(state));
    Ok(())
}

pub(super) fn initialize_server_command_state(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    registry: &PluginRegistry,
    runtime_owner: Option<&crate::runtime_owner::RuntimeOwner>,
) -> Result<()> {
    let state = build_runtime_command_state(
        config.clone(),
        paths.clone(),
        Arc::new(registry.clone()),
        runtime_owner,
    )?;
    RUNTIME_COMMAND_STATE_CACHE.with(|slot| *slot.borrow_mut() = Some(state));
    Ok(())
}

pub(super) struct SelectedCommandStateGuard(Option<RuntimeCommandState>);

impl SelectedCommandStateGuard {
    pub(super) fn enter(state: &RuntimeCommandState) -> Self {
        Self(RUNTIME_COMMAND_STATE_CACHE.with(|slot| slot.replace(Some(state.clone()))))
    }
}

impl Drop for SelectedCommandStateGuard {
    fn drop(&mut self) {
        RUNTIME_COMMAND_STATE_CACHE.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}

pub(super) fn selected_command_state() -> Result<RuntimeCommandState> {
    RUNTIME_COMMAND_STATE_CACHE
        .with(|slot| slot.borrow().clone())
        .context("no explicitly initialized command runtime for scoped CLI executor")
}

fn runtime_command_state() -> Result<RuntimeCommandState> {
    RUNTIME_COMMAND_STATE_CACHE.with(|slot| {
        if let Some(state) = slot.borrow().clone() {
            return Ok(state);
        }

        let config_started = Instant::now();
        let config = BmuxConfig::load()?;
        let paths = ConfigPaths::default();
        let config_us = config_started.elapsed().as_micros();
        let scan_started = Instant::now();
        let registry = Arc::new(scan_available_plugins(&config, &paths)?);
        let scan_us = scan_started.elapsed().as_micros();
        let build_started = Instant::now();
        // A lazily rebuilt state has no runtime owner: only an entry point that
        // owns the primary runtime can select plugin execution, explicitly.
        let state = build_runtime_command_state(config, paths, registry, None)?;
        emit_runtime_state_phase_timing(config_us, scan_us, build_started.elapsed().as_micros());
        *slot.borrow_mut() = Some(state.clone());
        Ok(state)
    })
}

pub(super) fn build_runtime_command_state(
    config: BmuxConfig,
    paths: ConfigPaths,
    registry: Arc<PluginRegistry>,
    runtime_owner: Option<&crate::runtime_owner::RuntimeOwner>,
) -> Result<RuntimeCommandState> {
    let enabled_plugins = effective_enabled_plugins(&config, &registry);
    let available_capability_providers = available_capability_providers(&config, &registry)?;
    let available_capability_names = available_capability_providers
        .keys()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let available_services = available_service_descriptors(&config, &registry)?;
    let plugin_search_roots = resolve_plugin_search_paths(&config, &paths)?
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let registered_plugin_infos = registered_plugin_infos_from_registry(&registry);
    let state = RuntimeCommandState {
        runtime_owner: runtime_owner.map(crate::runtime_owner::RuntimeOwner::downgrade),
        config,
        paths,
        registry,
        enabled_plugins,
        available_capability_providers,
        available_capability_names,
        available_services,
        plugin_search_roots,
        registered_plugin_infos,
    };
    mark_runtime_command_state_remote(&state);
    Ok(state)
}

fn mark_runtime_command_state_remote(state: &RuntimeCommandState) {
    // Standalone CLI processes see enabled plugins as server-owned providers;
    // server activation can still override entries to Local later.
    let locations = bmux_plugin::global_service_locations();
    for plugin_id in &state.enabled_plugins {
        locations.mark_remote(plugin_id);
    }
}

fn emit_runtime_state_phase_timing(config_us: u128, scan_us: u128, build_us: u128) {
    let payload = PhasePayload::new("runtime_command_state")
        .field("config_us", config_us)
        .field("scan_us", scan_us)
        .field("build_us", build_us)
        .field("total_us", config_us + scan_us + build_us)
        .finish();
    emit_phase_timing(PhaseChannel::Plugin, &payload);
}

fn emit_plugin_runtime_phase_timing(payload: &serde_json::Value) {
    emit_phase_timing(PhaseChannel::Plugin, payload);
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

/// Look up the `accepts_repeat` flag for a plugin command through the
/// process-level command state cache.
///
/// Called from the attach runtime's repeat filter
/// (`action_supports_repeat`) to decide whether a
/// `RuntimeAction::PluginCommand` should fire on `KeyEventKind::Repeat`
/// events. Returns `false` when state is not initialized or the plugin
/// / command is unknown — the conservative choice matches the default
/// behavior of filtering mutating commands out of repeat.
pub(super) fn command_accepts_repeat(plugin_id: &str, command_name: &str) -> bool {
    runtime_command_state().is_ok_and(|state| {
        state
            .registry
            .command_accepts_repeat(plugin_id, command_name)
    })
}

pub(super) fn bundled_plugin_root() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let parent = executable.parent()?;
    Some(parent.join("plugins"))
}

pub(super) fn workspace_bundled_plugin_root() -> Option<PathBuf> {
    if !cfg!(debug_assertions) {
        return None;
    }
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
        .is_some_and(|path| path.is_file())
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
        .filter(|&plugin| plugin.bundled_static && plugin.manifest.enabled_by_default)
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
                && plugin.manifest.enabled_by_default
                && (bundled_roots.contains(&plugin.search_root)
                    || matches!(
                        config.plugins.sources.get(plugin.declaration.id.as_str()),
                        Some(PluginSourceConfig::Dynamic { .. })
                    ))
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

/// Install client-side adapters owned by enabled bundled plugins.
///
/// Server plugin activation and client attach-provider registration have
/// different lifetimes. This hook derives client registration from the same
/// effective enabled-plugin set so disabled plugins cannot claim targets.
pub(super) fn install_enabled_client_adapters(
    config: &BmuxConfig,
    registry: &PluginRegistry,
    paths: &ConfigPaths,
) {
    // A logical ID match is insufficient: never install this executable's
    // bundled frontend against a dynamically selected implementation. Frontend
    // installation must come from the selected instance's negotiated contracts.
    let bundled_enabled = effective_enabled_plugins(config, registry)
        .into_iter()
        .filter(|id| registry.get(id).is_some_and(|plugin| plugin.bundled_static))
        .collect::<Vec<_>>();
    install_static_bundled_client_adapters(&bundled_enabled, paths);
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

/// Server deployment does not activate frontend-only code. The full registry
/// remains available to attach/command selection; explicit wrong-role requests
/// fail instead of substituting another implementation or component.
pub(super) fn backend_enabled_plugins(
    config: &BmuxConfig,
    registry: &PluginRegistry,
) -> Result<Vec<String>> {
    let mut backend = Vec::new();
    for id in effective_enabled_plugins(config, registry) {
        if let Some(plugin) = registry.get(&id)
            && !plugin
                .manifest
                .component_roles
                .contains(&bmux_plugin_sdk::definition::ComponentRole::Backend)
        {
            anyhow::ensure!(
                !config.plugins.sources.contains_key(&id) && !config.plugins.enabled.contains(&id),
                "plugin '{id}' was explicitly selected for server startup but does not export Backend; deploy its Frontend component in the attach host"
            );
            continue;
        }
        backend.push(id);
    }
    Ok(backend)
}

// Dependency ordering, load failures and shared startup cache must agree.
#[allow(clippy::too_many_lines)]
pub(super) fn load_enabled_plugins(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    registry: &PluginRegistry,
    host_state: &Arc<bmux_plugin::PluginStateRegistry>,
    definitions: &BTreeMap<String, bmux_plugin_sdk::definition::PluginDefinition>,
    runtime_owner: Option<&crate::runtime_owner::RuntimeOwner>,
) -> Result<Vec<ProductionPlugin>> {
    let total_started = Instant::now();
    let approvals = kernel_policy::load_native_approvals(paths)?;
    let enabled_plugins = backend_enabled_plugins(config, registry)?;
    if enabled_plugins.is_empty() {
        emit_plugin_runtime_phase_timing(
            &PhasePayload::new("plugin.load_all")
                .field("enabled_count", 0_usize)
                .field("loaded_count", 0_usize)
                .field("total_us", total_started.elapsed().as_micros())
                .finish(),
        );
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
        .chain(config.plugins.sources.keys())
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
    // One authority per startup, not per OS thread or logical plugin name.
    let runtime_owner = runtime_owner.cloned();
    let runtime = bmux_plugin_runtime::PluginRuntimeScopeId::new();
    let artifact_loader = bmux_plugin::NativePluginLoader::new();
    LOADED_PLUGIN_CACHE.with(|cache| cache.borrow_mut().clear());
    for plugin in ordered_plugins {
        let plugin_id = plugin.declaration.id.as_str();
        let load_started = Instant::now();
        let backend = if plugin.bundled_static {
            "static"
        } else if plugin.manifest.runtime == PluginRuntime::Process {
            "process"
        } else {
            "dynamic"
        };
        tracing::info!(plugin_id, backend, "server.startup.plugin.load.begin");
        let loaded = match ProductionPlugin::load_component(
            plugin,
            &host,
            &available_capabilities,
            config,
            selected::ExecutionOwner {
                cli_command: None,
                runtime_owner: runtime_owner.clone(),
                runtime,
                host_state: Arc::clone(host_state),
                loader: &artifact_loader,
                definitions,
                approval_store: approvals.as_ref(),
                runtime_dir: &paths.runtime_dir,
            },
            bmux_plugin_sdk::definition::ComponentRole::Backend,
        ) {
            Ok(loaded) => {
                emit_plugin_runtime_phase_timing(
                    &PhasePayload::new("plugin.load")
                        .field("plugin_id", plugin_id)
                        .field("backend", backend)
                        .field("bundled_static", plugin.bundled_static)
                        .field("total_us", load_started.elapsed().as_micros())
                        .finish(),
                );
                loaded
            }
            Err(error) => {
                emit_plugin_runtime_phase_timing(
                    &PhasePayload::new("plugin.load")
                        .field("plugin_id", plugin_id)
                        .field("backend", backend)
                        .field("bundled_static", plugin.bundled_static)
                        .field("error", error.to_string())
                        .field("total_us", load_started.elapsed().as_micros())
                        .finish(),
                );
                // Every member of this set was selected for backend deployment.
                // An invalid present provider must not become an apparently
                // healthy missing-plugin installation (notably authorization).
                return Err(error)
                    .with_context(|| format!("failed loading enabled plugin '{plugin_id}'"));
            }
        };
        // Command routing in this host must observe exactly the startup
        // implementation, not invoke its factory a second time.
        LOADED_PLUGIN_CACHE.with(|cache| {
            cache
                .borrow_mut()
                .insert(plugin_id.to_string(), Rc::new(loaded.clone()));
        });
        loaded_plugins.push(loaded);
    }

    emit_plugin_runtime_phase_timing(
        &PhasePayload::new("plugin.load_all")
            .field("enabled_count", enabled_plugins.len())
            .field("loaded_count", loaded_plugins.len())
            .field("total_us", total_started.elapsed().as_micros())
            .finish(),
    );

    Ok(loaded_plugins)
}

pub(super) fn registered_plugin_infos_from_loaded(
    loaded_plugins: &[ProductionPlugin],
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
            command_schemas: plugin.declaration.commands.clone(),
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
            command_schemas: plugin.declaration.commands.clone(),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
/// Capture an in-process caller for the existing execution service, without a
/// transport client that could itself affect companion idleness.
pub(super) fn managed_companion_caller(
    loaded_plugins: &[ProductionPlugin],
    config: &BmuxConfig,
    paths: &ConfigPaths,
    bridge: bmux_plugin_sdk::HostKernelBridge,
    host_context: bmux_server::ServiceInvokeContext,
) -> Result<bmux_plugin::TypedServiceCaller> {
    let plugin = loaded_plugins
        .iter()
        .find(|plugin| plugin.declaration.id.as_str() == "bmux.pane_runtime")
        .context("managed companion requires execution lifecycle service")?;
    let services = service_descriptors_from_declarations(
        loaded_plugins.iter().map(|plugin| &plugin.declaration),
    );
    let capabilities = core_provided_capabilities()
        .into_iter()
        .chain(
            loaded_plugins
                .iter()
                .flat_map(|plugin| plugin.declaration.provided_capabilities.iter().cloned()),
        )
        .map(|capability| capability.to_string())
        .collect();
    let enabled = loaded_plugins
        .iter()
        .map(|plugin| plugin.declaration.id.as_str().to_owned())
        .collect();
    let roots = resolve_plugin_search_paths(config, paths)?
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let mut context = plugin_lifecycle_context(
        config,
        paths,
        &plugin.declaration,
        PluginLifecycleAvailability {
            services,
            capabilities,
            enabled_plugins: enabled,
            search_roots: roots,
            registered_plugins: registered_plugin_infos_from_loaded(loaded_plugins),
        },
    );
    context.host_kernel_bridge = Some(bridge);
    // Typed calls go through the server's own service dispatch on the host
    // context, attributed to the execution plugin this watcher acts for. The
    // legacy in-process resolver cannot reach scoped startup instances (it
    // tries to load the provider natively and fails), so it is never the route.
    let authority = super::plugin_kernel::AuthenticatedServiceInvocation::new(host_context)
        .with_plugin_caller(&plugin.declaration.id);
    context.host_service_port = Some(bmux_plugin_sdk::PluginHostServices::new(move |request| {
        let protocol = |details: String| bmux_plugin_sdk::PluginError::ServiceProtocol { details };
        let kind = super::plugin_kernel::invoke_kind_from_service_kind(request.kind)
            .ok_or_else(|| protocol("managed watcher cannot invoke event services".into()))?;
        let encoded = bmux_ipc::encode(&bmux_ipc::Request::InvokeService {
            capability: request.capability.to_owned(),
            kind,
            interface_id: request.interface.to_owned(),
            operation: request.operation.to_owned(),
            payload: request.payload.to_vec(),
        })
        .map_err(|error| protocol(error.to_string()))?;
        match authority.execute_response(&encoded)? {
            bmux_ipc::ResponsePayload::ServiceInvoked { payload } => Ok(payload),
            _ => Err(protocol(
                "unexpected response to managed watcher service call".into(),
            )),
        }
    }));
    Ok(bmux_plugin::TypedServiceCaller::from_lifecycle_context(
        &context,
    ))
}

/// Host inventory visible to a plugin during lifecycle dispatch.
struct PluginLifecycleAvailability {
    services: Vec<RegisteredService>,
    capabilities: Vec<String>,
    enabled_plugins: Vec<String>,
    search_roots: Vec<String>,
    registered_plugins: Vec<bmux_plugin_sdk::RegisteredPluginInfo>,
}

fn plugin_lifecycle_context(
    config: &BmuxConfig,
    paths: &ConfigPaths,
    declaration: &bmux_plugin::PluginDeclaration,
    availability: PluginLifecycleAvailability,
) -> NativeLifecycleContext {
    let PluginLifecycleAvailability {
        services: available_services,
        capabilities: available_capabilities,
        enabled_plugins,
        search_roots: plugin_search_roots,
        registered_plugins,
    } = availability;
    let host = plugin_host_for_declaration(declaration, paths, config, available_services.clone());
    NativeLifecycleContext {
        auxiliary_executables: BTreeMap::default(),
        host_service_port: None,
        capabilities: bmux_plugin_sdk::definition::ActivationCapabilities::default(),
        auxiliary_executable_errors: BTreeMap::default(),
        plugin_id: declaration.id.as_str().to_string(),
        required_capabilities: declaration
            .required_capabilities
            .iter()
            .chain(declaration.optional_capabilities.iter())
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
    active_keybindings: Vec<bmux_plugin_sdk::ActiveKeybinding>,
    caller_client_id: Option<uuid::Uuid>,
    invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource,
) -> NativeCommandContext {
    let host = plugin_host_for_declaration(declaration, paths, config, available_services.clone());
    NativeCommandContext {
        host_service_port: None,
        plugin_id: declaration.id.as_str().to_string(),
        command: command.to_string(),
        arguments: arguments.to_vec(),
        required_capabilities: declaration
            .required_capabilities
            .iter()
            .chain(declaration.optional_capabilities.iter())
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
        active_keybindings,
        host: plugin_host_metadata(),
        connection: bmux_plugin_sdk::PluginHost::connection(&host).clone(),
        settings: config
            .plugins
            .settings
            .get(declaration.id.as_str())
            .cloned(),
        plugin_settings_map: config.plugins.settings.clone(),
        caller_client_id,
        invocation_source: if invocation_source
            == bmux_plugin_sdk::NativeCommandInvocationSource::Cli
            && caller_client_id.is_none()
        {
            bmux_plugin_sdk::NativeCommandInvocationSource::LocalProcess
        } else {
            invocation_source
        },
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
// Kept together so typed-service context construction, registry install, and
// readiness declaration remain auditable as one startup phase.
#[allow(clippy::too_many_lines)]
fn install_typed_service_registry(
    loaded_plugins: &[ProductionPlugin],
    config: &BmuxConfig,
    paths: &ConfigPaths,
) -> Result<()> {
    let total_started = Instant::now();
    // Build a bridge that plugins may stash inside their typed
    // service handles. It shares the same dispatch function as the
    // per-activation bridge in `plugin_lifecycle_context`, so host
    // calls from trait methods reach the same kernel routing as
    // host calls from `invoke_service` / `run_command`.

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
        // Dynamic components use portable service dispatch, not native TypeId.
        if !plugin.registered.bundled_static {
            continue;
        }
        let collect_started = Instant::now();
        let required_caps: Vec<String> = plugin
            .declaration
            .required_capabilities
            .iter()
            .chain(plugin.declaration.optional_capabilities.iter())
            .map(ToString::to_string)
            .collect();
        let provided_caps: Vec<String> = plugin
            .declaration
            .provided_capabilities
            .iter()
            .map(ToString::to_string)
            .collect();
        let context = bmux_plugin_sdk::TypedServiceRegistrationContext {
            host_service_port: None,
            plugin_id: plugin.declaration.id.as_str(),
            host_kernel_bridge: None,
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
        let entries = plugin
            .collect_typed_services(context)
            .with_context(|| {
                format!(
                    "failed collecting active typed registrations for '{}'",
                    plugin.declaration.id.as_str()
                )
            })?
            .into_entries();
        let service_count = entries.len();
        for (key, handle) in entries {
            ensure_typed_service_declared(plugin, &key)?;
            map.insert(key, handle);
        }
        emit_plugin_runtime_phase_timing(
            &PhasePayload::new("plugin.typed_services.collect")
                .field("plugin_id", plugin.declaration.id.as_str())
                .field("service_count", service_count)
                .field("total_us", collect_started.elapsed().as_micros())
                .finish(),
        );
    }
    let total_service_count = map.len();
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
    emit_plugin_runtime_phase_timing(
        &PhasePayload::new("plugin.typed_services.collect_all")
            .field("plugin_count", loaded_plugins.len())
            .field("service_count", total_service_count)
            .field("total_us", total_started.elapsed().as_micros())
            .finish(),
    );
    Ok(())
}

fn ensure_typed_service_declared(
    plugin: &ProductionPlugin,
    key: &bmux_plugin_sdk::TypedServiceKey,
) -> Result<()> {
    let (capability, kind, interface_id) = key;
    if plugin.declaration.services.iter().any(|service| {
        service.capability == *capability
            && service.kind == *kind
            && service.interface_id == interface_id.as_str()
    }) {
        return Ok(());
    }
    anyhow::bail!(
        "plugin '{}' registered typed service capability='{}' kind='{kind:?}' interface='{}' but its effective declaration does not expose it; provide the interface from the plugin's BPDL world",
        plugin.declaration.id.as_str(),
        capability.as_str(),
        interface_id.as_str(),
    );
}

/// Flip every ready signal declared by `plugin` to
/// [`bmux_plugin_sdk::ReadyStatus::Ready`]. Called after the plugin's
/// `activate` returns `Ok` to signal that any subsystem waiting on one
/// of its signals can proceed. Plugins that need more granular control
/// (e.g. waiting on async work beyond activation) will be extended
/// with an explicit `mark_ready` host-kernel bridge operation later.
fn mark_plugin_ready_signals(plugin: &ProductionPlugin) {
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

/// Resolve the typed `tabs-commands` service handle from the current
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
    std::sync::Arc<dyn bmux_tabs_plugin_api::tabs_commands::TabsCommandsService + Send + Sync>,
> {
    let write_cap = bmux_plugin_sdk::HostScope::new("bmux.tabs.write").ok()?;
    let registry = typed_service_registry_snapshot();
    let handle = registry.get(&(
        write_cap,
        bmux_plugin_sdk::ServiceKind::Command,
        bmux_tabs_plugin_api::tabs_commands::INTERFACE_ID,
    ))?;
    handle
        .provider_as_trait::<
            dyn bmux_tabs_plugin_api::tabs_commands::TabsCommandsService + Send + Sync,
        >()
        .ok()
}

/// Resolve the typed `tabs-state` service handle from the current
/// registry. Mirrors [`resolve_windows_commands_service`] for the
/// read-only query interface.
#[must_use]
#[allow(dead_code)] // Consumed by attach-flow routing changes landing in a follow-up.
pub(super) fn resolve_windows_state_service()
-> Option<std::sync::Arc<dyn bmux_tabs_plugin_api::tabs_state::TabsStateService + Send + Sync>> {
    let read_cap = bmux_plugin_sdk::HostScope::new("bmux.tabs.read").ok()?;
    let registry = typed_service_registry_snapshot();
    let handle = registry.get(&(
        read_cap,
        bmux_plugin_sdk::ServiceKind::Query,
        bmux_tabs_plugin_api::tabs_state::INTERFACE_ID,
    ))?;
    handle
        .provider_as_trait::<dyn bmux_tabs_plugin_api::tabs_state::TabsStateService + Send + Sync>()
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

/// Minimal single-threaded executor for driving a typed-dispatch
/// future to completion from synchronous code.
///
/// Typed service method futures returned by the BPDL-generated trait
/// are `Pin<Box<dyn Future + Send>>`. Runtime consumers that live
/// inside synchronous call paths can use this helper without pulling
/// in a full tokio runtime. Typed-service futures are `async move
/// { ... }` blocks that never suspend, so `Poll::Pending` shouldn't
/// be reachable in practice; if it is observed we spin briefly and
/// yield rather than hanging the thread.
#[allow(dead_code)] // Kept for future sync-call-site typed dispatch (no current consumers).
fn block_on_future<T>(
    fut: std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + '_>>,
) -> T {
    use std::task::{Context, Poll};

    let waker = std::task::Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut pinned = fut;
    loop {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

// Kept together so activation, rollback, readiness, and service-location state
// stay consistent across all plugins.
#[allow(clippy::too_many_lines)]
pub(super) async fn activate_loaded_plugins(
    loaded_plugins: &[ProductionPlugin],
    config: &BmuxConfig,
    paths: &ConfigPaths,
) -> Result<()> {
    let total_started = Instant::now();
    // Snapshot the typed services each plugin exposes before we
    // activate anyone; bundled plugins can have their typed handles
    // harvested without side effects, and consumers that resolve a
    // typed service observe a stable registry across the lifetime of
    // the attach session.
    // Collect typed adapters only after their owning activation is active.

    let mut activated: Vec<&ProductionPlugin> = Vec::new();
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
    let mut activated_count = 0_usize;
    for plugin in loaded_plugins {
        if !plugin.declaration.lifecycle.activate_on_startup {
            continue;
        }

        let activate_started = Instant::now();
        let context = plugin_lifecycle_context(
            config,
            paths,
            &plugin.declaration,
            PluginLifecycleAvailability {
                services: available_services.clone(),
                capabilities: available_capabilities.clone(),
                enabled_plugins: enabled_plugins.clone(),
                search_roots: plugin_search_roots.clone(),
                registered_plugins: registered_plugins.clone(),
            },
        );
        let activation_connection_guard = enter_host_kernel_connection(connection_info.clone());
        // `map_or_else` trips `result_large_err` here because plugin activation
        // returns the repository-wide `PluginError`; the explicit match keeps the
        // large error type out of closure signatures.
        tracing::info!(
            plugin_id = plugin.declaration.id.as_str(),
            "server.startup.plugin.activate.begin"
        );
        let activate_result = plugin.activate_async(&context).await;
        tracing::info!(
            plugin_id = plugin.declaration.id.as_str(),
            succeeded = activate_result.is_ok(),
            "server.startup.plugin.activate.complete"
        );
        drop(activation_connection_guard);
        if let Err(error) = activate_result {
            plugin.cancel_background_tasks()?;
            for activated_plugin in &activated {
                activated_plugin.cancel_background_tasks()?;
            }
            if let Err(cleanup_error) = plugin.drain_background_tasks().await {
                return Err(cleanup_error).context(format!(
                    "activation failed ({error}); failed draining partial activation"
                ));
            }
            for activated_plugin in &activated {
                activated_plugin.drain_background_tasks().await?;
            }
            {
                let _connection = enter_host_kernel_connection(connection_info.clone());
                if let Err(cleanup) = plugin.deactivate_async(&context).await {
                    return Err(anyhow::anyhow!(
                        "activation failed for '{}': {error}; partial activation cleanup failed: {cleanup}",
                        plugin.declaration.id.as_str(),
                    ));
                }
            }
            emit_plugin_runtime_phase_timing(
                &PhasePayload::new("plugin.lifecycle.activate")
                    .field("plugin_id", plugin.declaration.id.as_str())
                    .field("ready_signal_count", plugin.declaration.ready_signals.len())
                    .field("error", error.to_string())
                    .field("total_us", activate_started.elapsed().as_micros())
                    .finish(),
            );
            for activated_plugin in activated.into_iter().rev() {
                let context = plugin_lifecycle_context(
                    config,
                    paths,
                    &activated_plugin.declaration,
                    PluginLifecycleAvailability {
                        services: available_services.clone(),
                        capabilities: available_capabilities.clone(),
                        enabled_plugins: enabled_plugins.clone(),
                        search_roots: plugin_search_roots.clone(),
                        registered_plugins: registered_plugins.clone(),
                    },
                );
                activated_plugin.drain_background_tasks().await?;
                let _host_kernel_connection_guard =
                    enter_host_kernel_connection(connection_info.clone());
                let deactivate_result = activated_plugin.deactivate_async(&context).await;
                if let Err(deactivate_error) = deactivate_result {
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

        emit_plugin_runtime_phase_timing(
            &PhasePayload::new("plugin.lifecycle.activate")
                .field("plugin_id", plugin.declaration.id.as_str())
                .field("ready_signal_count", plugin.declaration.ready_signals.len())
                .field("status", 0_i32)
                .field("total_us", activate_started.elapsed().as_micros())
                .finish(),
        );
        activated_count += 1;
        activated.push(plugin);
        mark_plugin_ready_signals(plugin);
        // Record that this process holds the activated provider for
        // `plugin`'s typed services. Plugin-to-plugin typed calls
        // consult the `ServiceLocationMap` and stay in-process when
        // the target is `Local`; remote callers (attach / CLI) mark
        // the same plugins as `Remote` so their typed calls forward
        // here via `Request::InvokeService`.
        bmux_plugin::global_service_locations().mark_local(plugin.declaration.id.as_str());
    }

    install_typed_service_registry(loaded_plugins, config, paths)?;
    emit_plugin_runtime_phase_timing(
        &PhasePayload::new("plugin.lifecycle.activate_all")
            .field("plugin_count", loaded_plugins.len())
            .field("activated_count", activated_count)
            .field("total_us", total_started.elapsed().as_micros())
            .finish(),
    );

    Ok(())
}

pub(super) async fn deactivate_loaded_plugins(
    loaded_plugins: &[ProductionPlugin],
    config: &BmuxConfig,
    paths: &ConfigPaths,
    host_states: &bmux_plugin::PluginStateRegistry,
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
    // Stop every producer while all service providers are still alive.
    for plugin in loaded_plugins {
        plugin.cancel_background_tasks()?;
    }
    let drains = loaded_plugins.iter().map(|plugin| async move {
        tracing::info!(plugin_id = plugin.declaration.id.as_str(), "shutdown background drain started");
        let result = plugin.drain_background_tasks().await;
        if let Err(error) = host_states.shutdown_progress().advance("plugin background scope drained") { tracing::warn!(%error, "shutdown progress publication failed"); }
        tracing::info!(plugin_id = plugin.declaration.id.as_str(), error = ?result.as_ref().err(), "shutdown background drain completed");
        result
    });
    let mut failures = Vec::new();
    for result in futures_util::future::join_all(drains).await {
        if let Err(error) = result {
            failures.push(error.to_string());
        }
    }
    anyhow::ensure!(
        failures.is_empty(),
        "background cleanup failed: {}",
        failures.join("; ")
    );
    for plugin in loaded_plugins.iter().rev() {
        if !plugin.declaration.lifecycle.activate_on_startup {
            continue;
        }

        let context = plugin_lifecycle_context(
            config,
            paths,
            &plugin.declaration,
            PluginLifecycleAvailability {
                services: available_services.clone(),
                capabilities: available_capabilities.clone(),
                enabled_plugins: enabled_plugins.clone(),
                search_roots: plugin_search_roots.clone(),
                registered_plugins: registered_plugins.clone(),
            },
        );
        plugin.drain_background_tasks().await?;
        let _host_kernel_connection_guard = enter_host_kernel_connection(connection_info.clone());
        let deactivate_result = plugin.deactivate_async(&context).await;
        let _ = deactivate_result.with_context(|| {
            format!(
                "failed deactivating plugin '{}'",
                plugin.declaration.id.as_str()
            )
        })?;
        if let Err(error) = host_states
            .shutdown_progress()
            .advance("plugin deactivated")
        {
            tracing::warn!(%error, "shutdown progress publication failed");
        }
    }

    Ok(())
}

pub(super) fn dispatch_loaded_plugin_event(
    loaded_plugins: &[ProductionPlugin],
    event: &PluginEvent,
) -> Result<()> {
    for plugin in loaded_plugins {
        let started = std::time::Instant::now();
        tracing::debug!(
            plugin_id = plugin.declaration.id.as_str(),
            event_kind = event.kind.as_str(),
            "plugin event dispatch started"
        );
        let _ = plugin.dispatch_event(event).with_context(|| {
            format!(
                "failed dispatching plugin event '{}' to '{}'",
                event.kind.as_str(),
                plugin.declaration.id.as_str()
            )
        })?;
        tracing::debug!(
            plugin_id = plugin.declaration.id.as_str(),
            event_kind = event.kind.as_str(),
            elapsed_ms = started.elapsed().as_millis(),
            "plugin event dispatch completed"
        );
    }

    Ok(())
}

pub(super) async fn plugin_event_bridge_loop(
    loaded_plugins: std::sync::Arc<Vec<ProductionPlugin>>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    if loaded_plugins.is_empty() {
        return Ok(());
    }

    loop {
        if *shutdown_rx.borrow() {
            return Ok(());
        }

        let client = loop {
            match connect_raw("bmux-plugin-event-bridge").await {
                Ok(client) => break client,
                Err(error) => {
                    debug!(%error, "plugin event bridge connection failed; retrying");
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

        // Upgrade to streaming client for server-push event delivery. A bridge
        // disconnect is not a server-lifetime signal: reconnect until explicit
        // shutdown so transient IPC failures cannot stop the server.
        let Ok(mut streaming_client) = bmux_client::StreamingBmuxClient::from_client(client) else {
            warn!("plugin event bridge streaming upgrade failed; retrying");
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        return Ok(());
                    }
                }
                () = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
            continue;
        };
        if let Err(error) = streaming_client.subscribe_events().await {
            warn!(%error, "plugin event bridge subscription failed; retrying");
            continue;
        }
        if let Err(error) = streaming_client.enable_event_push().await {
            warn!(%error, "plugin event bridge event-push enable failed; retrying");
            continue;
        }

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        return Ok(());
                    }
                }
                event = streaming_client.event_receiver().recv() => {
                    let Some(event) = event else {
                        warn!("plugin event bridge disconnected; retrying");
                        break;
                    };
                    // Skip high-frequency pane output notifications for plugins.
                    if is_pane_output_plugin_event(&event) {
                        continue;
                    }
                    let plugins = std::sync::Arc::clone(&loaded_plugins);
                    let event = plugin_event_from_server_event(&event)?;
                    // A synchronous plugin callback can wait on services backed by
                    // server.run(). Never block that sibling future's polling task.
                    if let Err(error) = dispatch_plugin_callback(move || {
                        dispatch_loaded_plugin_event(&plugins, &event)
                    }).await? {
                        warn!(%error, "failed dispatching bridged plugin event");
                    }
                }
            }
        }
    }
}

async fn dispatch_plugin_callback<F>(callback: F) -> Result<Result<()>>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    // Event callbacks run on a blocking worker. Preserve the explicitly pinned
    // attach configuration there too; never let a callback discover user roots.
    let state = RUNTIME_COMMAND_STATE_CACHE.with(|slot| slot.borrow().clone());
    tokio::task::spawn_blocking(move || {
        struct RestoreState(Option<RuntimeCommandState>);
        impl Drop for RestoreState {
            fn drop(&mut self) {
                RUNTIME_COMMAND_STATE_CACHE.with(|slot| *slot.borrow_mut() = self.0.take());
            }
        }
        let _restore = RestoreState(
            RUNTIME_COMMAND_STATE_CACHE
                .with(|slot| std::mem::replace(&mut *slot.borrow_mut(), state)),
        );
        callback()
    })
    .await
    .context("join plugin event dispatch")
}

fn is_pane_output_plugin_event(event: &bmux_client::ServerEvent) -> bool {
    let bmux_client::ServerEvent::PluginBusEvent { kind, payload } = event else {
        return false;
    };
    if kind != bmux_pane_runtime_plugin_api::pane_runtime_events::EVENT_KIND.as_str() {
        return false;
    }
    serde_json::from_slice::<bmux_pane_runtime_plugin_api::pane_runtime_events::PaneEvent>(payload)
        .is_ok_and(|event| {
            matches!(
                event,
                bmux_pane_runtime_plugin_api::pane_runtime_events::PaneEvent::OutputAvailable { .. }
            )
        })
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
    let state = runtime_command_state()?;
    run_plugin_command_with_state(&state, plugin_id, command_name, args).await
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Status clamped to 0..255 before cast
pub(super) async fn run_plugin_command_with_state(
    state: &RuntimeCommandState,
    plugin_id: &str,
    command_name: &str,
    args: &[String],
) -> Result<u8> {
    // Resolve once without requiring a live daemon. A host-independent command
    // may run without opening any connection; an actual host request must use
    // this exact selected target or fail, never reconnect to a later default.
    let target =
        crate::connection::StopTarget::resolve(crate::connection::ConnectionContext::default())
            .await
            .map_err(|error| format!("selected command target unavailable: {error:#}"));
    let execution =
        run_command_on_selected_target(state, plugin_id, command_name, args, target).await?;
    if execution.status != 0
        && let Some(error) = execution.outcome.error_message.as_deref()
    {
        eprintln!("{error}");
    }
    Ok(execution.status.clamp(0, i32::from(u8::MAX)) as u8)
}

async fn run_command_on_selected_target(
    state: &RuntimeCommandState,
    plugin_id: &str,
    command_name: &str,
    args: &[String],
    target: std::result::Result<crate::connection::StopTarget, String>,
) -> Result<PluginCommandExecution> {
    let (dispatch, _owner, mut requests) = super::plugin_kernel::CommandDispatch::channel();
    let state = state.clone();
    let plugin_id = plugin_id.to_owned();
    let command_name = command_name.to_owned();
    let args = args.to_vec();
    let command = tokio::task::spawn_blocking(move || {
        run_plugin_command_internal_with_state(
            Some(&state),
            &plugin_id,
            &command_name,
            &args,
            None,
            None,
            Vec::new(),
            bmux_plugin_sdk::NativeCommandInvocationSource::Cli,
            Some(dispatch),
            None,
        )
    });
    let pump = tokio::spawn(async move {
        let mut client = None;
        let mut connection_failure = None;
        let mut command = command;
        let execution = loop {
            tokio::select! {
                result = &mut command => break result.context("selected command worker failed")?,
                Some(request) = requests.recv() => {
                    if request.is_cancelled() {
                        request.reject();
                    } else {
                        if client.is_none() && connection_failure.is_none() {
                            let connected = match &target {
                                Ok(target) => target.connect("plugin-command").await
                                    .map_err(|error| format!("selected command connection failed: {error:#}")),
                                Err(error) => Err(error.clone()),
                            };
                            match connected {
                                Ok(connected) => client = Some(connected),
                                Err(error) => connection_failure = Some(error),
                            }
                        }
                        if let Some(connection) = client.as_mut() {
                            let wire = request.wire_request(connection.negotiated_protocol());
                            let result = connection.request_raw(wire).await;
                            if let Err(error) = &result {
                                connection_failure = Some(format!("selected command transport failed; no reconnect: {error}"));
                            }
                            request.respond(result.map_err(anyhow::Error::from));
                            if connection_failure.is_some() { client = None; }
                        } else {
                            request.reject_unavailable(connection_failure.as_deref()
                                .unwrap_or("selected command connection unavailable"));
                        }
                    }
                }
            }
        };
        requests.close();
        while let Some(request) = requests.recv().await {
            request.reject();
        }
        execution
    });
    // Dropping this observer cancels new work through _owner, but never aborts
    // the connection-owning pump or an already-dispatched effect.
    pump.await.context("selected command pump failed")?
}

pub(super) fn run_plugin_keybinding_command(
    plugin_id: &str,
    command_name: &str,
    args: &[String],
    kernel_client_factory: Option<&KernelClientFactory>,
    caller_client_id: Option<uuid::Uuid>,
) -> Result<PluginCommandExecution> {
    run_plugin_keybinding_command_with_active_bindings(
        plugin_id,
        command_name,
        args,
        kernel_client_factory,
        caller_client_id,
        Vec::new(),
    )
}

pub(super) fn run_plugin_keybinding_command_with_active_bindings(
    plugin_id: &str,
    command_name: &str,
    args: &[String],
    kernel_client_factory: Option<&KernelClientFactory>,
    caller_client_id: Option<uuid::Uuid>,
    active_keybindings: Vec<bmux_plugin_sdk::ActiveKeybinding>,
) -> Result<PluginCommandExecution> {
    run_plugin_command_internal(
        plugin_id,
        command_name,
        args,
        kernel_client_factory,
        caller_client_id,
        active_keybindings,
        bmux_plugin_sdk::NativeCommandInvocationSource::AttachKeybinding,
    )
}

#[allow(clippy::too_many_arguments)] // The attach worker's selected route, caller and frontend owner are independent inputs.
pub(super) fn run_plugin_keybinding_command_with_dispatch(
    state: &RuntimeCommandState,
    plugin_id: &str,
    command_name: &str,
    args: &[String],
    caller_client_id: Option<uuid::Uuid>,
    active_keybindings: Vec<bmux_plugin_sdk::ActiveKeybinding>,
    dispatch: super::plugin_kernel::CommandDispatch,
    frontends: Option<&OwnedFrontends>,
) -> Result<PluginCommandExecution> {
    run_plugin_command_internal_with_state(
        Some(state),
        plugin_id,
        command_name,
        args,
        None,
        caller_client_id,
        active_keybindings,
        bmux_plugin_sdk::NativeCommandInvocationSource::AttachKeybinding,
        Some(dispatch),
        frontends.map(CommandOwner::Frontends),
    )
}

pub(super) fn run_plugin_command_with_selected_dispatch(
    state: &RuntimeCommandState,
    request: &bmux_plugin_sdk::PluginCliCommandRequest,
    dispatch: super::plugin_kernel::CommandDispatch,
) -> Result<PluginCommandExecution> {
    run_plugin_command_internal_with_state(
        Some(state),
        &request.plugin_id,
        &request.command_name,
        &request.arguments,
        None,
        None,
        Vec::new(),
        bmux_plugin_sdk::NativeCommandInvocationSource::Cli,
        Some(dispatch),
        None,
    )
}

/// Run a plugin command requested by an authenticated server caller, such as
/// the core CLI bridge service. The command's host services dispatch through
/// this exact server invocation context, so they observe the requesting
/// client and stream; no cached lifecycle authority, ambient kernel context or
/// CLI transport stands in for it.
pub(super) fn run_plugin_command_with_service_authority(
    state: &RuntimeCommandState,
    request: &bmux_plugin_sdk::PluginCliCommandRequest,
    caller_client_id: Option<uuid::Uuid>,
    authority: super::plugin_kernel::AuthenticatedServiceInvocation,
    startup_instances: &[ProductionPlugin],
) -> Result<PluginCommandExecution> {
    // A backend plugin runs on the very instance the server started: its state,
    // host registry and runtime identity are the ones its providers use. Only
    // a plugin this host never started (no Backend role) is loaded per command.
    let instance = startup_instances
        .iter()
        .find(|loaded| loaded.declaration.id.as_str() == request.plugin_id);
    run_plugin_command_internal_with_state(
        Some(state),
        &request.plugin_id,
        &request.command_name,
        &request.arguments,
        None,
        caller_client_id,
        Vec::new(),
        bmux_plugin_sdk::NativeCommandInvocationSource::AttachKeybinding,
        None,
        Some(CommandOwner::Server(ServerCommandAuthority {
            authority,
            instance,
        })),
    )
}

/// Who owns the instance a command runs on, when not a fresh command-local one.
pub(super) enum CommandOwner<'a> {
    Server(ServerCommandAuthority<'a>),
    /// A host's owned frontends (e.g. one attachment's), activated with that
    /// host's activation binder for the host's lifetime.
    Frontends(&'a OwnedFrontends),
}
impl<'a> CommandOwner<'a> {
    fn split(
        owner: Option<Self>,
    ) -> (
        Option<super::plugin_kernel::AuthenticatedServiceInvocation>,
        Option<&'a ProductionPlugin>,
        Option<&'a OwnedFrontends>,
    ) {
        match owner {
            None => (None, None, None),
            Some(Self::Server(command)) => (Some(command.authority), command.instance, None),
            Some(Self::Frontends(frontends)) => (None, None, Some(frontends)),
        }
    }
}

/// A server-authenticated command request and the startup instance (if this
/// host started the plugin) that must execute it.
pub(super) struct ServerCommandAuthority<'a> {
    authority: super::plugin_kernel::AuthenticatedServiceInvocation,
    instance: Option<&'a ProductionPlugin>,
}

pub(super) fn run_plugin_keybinding_command_with_state(
    state: &RuntimeCommandState,
    plugin_id: &str,
    command_name: &str,
    args: &[String],
    kernel_client_factory: Option<&KernelClientFactory>,
    caller_client_id: Option<uuid::Uuid>,
) -> Result<PluginCommandExecution> {
    run_plugin_command_internal_with_state(
        Some(state),
        plugin_id,
        command_name,
        args,
        kernel_client_factory,
        caller_client_id,
        Vec::new(),
        bmux_plugin_sdk::NativeCommandInvocationSource::AttachKeybinding,
        None,
        None,
    )
}

#[derive(Debug, Clone)]
pub(super) struct PluginCommandPolicyHints {
    pub(super) execution: bmux_plugin_sdk::CommandExecutionKind,
    pub(super) execution_class: bmux_plugin::PluginExecutionClass,
    pub(super) required_capabilities: Vec<HostScope>,
}

pub(super) fn plugin_command_policy_hints(
    state: &RuntimeCommandState,
    plugin_id: &str,
    command_name: &str,
) -> Result<PluginCommandPolicyHints> {
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

#[instrument(
    level = "debug",
    target = "bmux_cli::runtime::plugin_command",
    skip_all,
    fields(
        plugin_id = %plugin_id,
        command_name = %command_name,
        invocation_source = ?invocation_source,
        args_count = args.len(),
        status = tracing::field::Empty,
        error = tracing::field::Empty,
    ),
)]
pub(super) fn run_plugin_command_internal(
    plugin_id: &str,
    command_name: &str,
    args: &[String],
    kernel_client_factory: Option<&KernelClientFactory>,
    caller_client_id: Option<uuid::Uuid>,
    active_keybindings: Vec<bmux_plugin_sdk::ActiveKeybinding>,
    invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource,
) -> Result<PluginCommandExecution> {
    run_plugin_command_internal_with_state(
        None,
        plugin_id,
        command_name,
        args,
        kernel_client_factory,
        caller_client_id,
        active_keybindings,
        invocation_source,
        None,
        None,
    )
}

fn bind_captured_command_route(
    context: &NativeCommandContext,
) -> Result<Option<bmux_plugin::AsyncCommandRouteGuard>> {
    bmux_plugin::capture_async_command_route(bmux_plugin::ASYNC_SERVICE_ROUTE_V1)
        .ok()
        .map(|route| {
            let services = context
                .services
                .iter()
                .filter(|service| {
                    context
                        .required_capabilities
                        .iter()
                        .chain(context.provided_capabilities.iter())
                        .any(|capability| capability == service.capability.as_str())
                })
                .cloned()
                .collect();
            route
                .with_services(services)
                .map(bmux_plugin::enter_async_command_route)
        })
        .transpose()
        .map_err(anyhow::Error::msg)
}

fn settle_command_cleanup<T>(work: impl std::future::Future<Output = T>) -> T {
    struct Wake(std::thread::Thread);
    impl std::task::Wake for Wake {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(Wake(std::thread::current())));
    let mut context = std::task::Context::from_waker(&waker);
    let mut work = std::pin::pin!(work);
    loop {
        match std::future::Future::poll(work.as_mut(), &mut context) {
            std::task::Poll::Ready(result) => return result,
            std::task::Poll::Pending => std::thread::park(),
        }
    }
}

#[allow(clippy::too_many_arguments)] // Command execution context is assembled from separate runtime subsystems.
fn run_plugin_command_internal_with_state(
    provided_state: Option<&RuntimeCommandState>,
    plugin_id: &str,
    command_name: &str,
    args: &[String],
    kernel_client_factory: Option<&KernelClientFactory>,
    caller_client_id: Option<uuid::Uuid>,
    active_keybindings: Vec<bmux_plugin_sdk::ActiveKeybinding>,
    invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource,
    command_dispatch: Option<super::plugin_kernel::CommandDispatch>,
    instance_owner: Option<CommandOwner<'_>>,
) -> Result<PluginCommandExecution> {
    let (command_authority, startup_instance, frontends) = CommandOwner::split(instance_owner);
    let total_started = Instant::now();
    let runtime_state_started = Instant::now();
    let cached_state;
    let state = if let Some(state) = provided_state {
        state
    } else {
        cached_state = runtime_command_state()?;
        &cached_state
    };
    // Select the caller's already-resolved snapshot before any factory load,
    // lifecycle admission or host CLI binding. This synchronous guard also
    // preserves the parent snapshot across nested selected command execution.
    let command_dispatch = command_dispatch.map(|dispatch| dispatch.with_state(state.clone()));
    let _selected_command_state = SelectedCommandStateGuard::enter(state);
    let runtime_state_us = runtime_state_started.elapsed().as_micros();
    let lookup_started = Instant::now();
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
    // Every entry point (CLI, keybinding, bridge, server) reaches the plugin
    // through this one place, so inline `--option=value` forms are accepted
    // for all plugin commands against their declared argument schema.
    let normalized_args = plugin
        .declaration
        .commands
        .iter()
        .find(|command| command.name == command_name)
        .and_then(|command| command.normalize_inline_option_values(args));
    let args = normalized_args.as_deref().unwrap_or(args);
    let lookup_us = lookup_started.elapsed().as_micros();

    let context_started = Instant::now();
    let context = plugin_command_context(
        config,
        paths,
        &plugin.declaration,
        command_name,
        args,
        state.available_services.clone(),
        state.available_capability_names.clone(),
        enabled_plugins,
        state.plugin_search_roots.clone(),
        state.registered_plugin_infos.clone(),
        active_keybindings,
        caller_client_id,
        invocation_source,
    );
    let context_us = context_started.elapsed().as_micros();
    let instance = command_instance(
        plugin,
        state,
        &context,
        startup_instance,
        frontends,
        command_dispatch.is_some(),
    )?;
    let _host_kernel_connection_guard = enter_host_kernel_connection(context.connection.clone());
    let _host_kernel_factory_guard =
        kernel_client_factory.map(|f| enter_host_kernel_client_factory(Arc::clone(f)));
    let _async_route = bind_captured_command_route(&context)?;
    let run_started = Instant::now();
    let run_result = instance.loaded.run_command_with_dispatch(
        command_name,
        args,
        Some(&context),
        command_dispatch.as_ref(),
        command_authority,
    );
    // The issuing route (and its callback parent lease) ends with the command
    // invocation; deactivation below binds its own lifecycle route.
    drop(command_dispatch);
    if instance.retire && run_result.is_ok() {
        retire_command_local_plugin(state, plugin, &instance.loaded)?;
    }
    let run_us = run_started.elapsed().as_micros();
    let (status, outcome) = run_result.map_err(|error| {
        // Record the error on the current span so structured tracing
        // preserves the full failure shape rather than only the
        // formatted message string the caller sees.
        tracing::Span::current().record("error", tracing::field::display(&error));
        anyhow::anyhow!(format_plugin_command_run_error(
            plugin_id,
            command_name,
            &error
        ))
    })?;
    tracing::Span::current().record("status", status);
    if let Some(msg) = outcome.error_message.as_deref() {
        tracing::Span::current().record("error", msg);
    }
    emit_plugin_phase_timing(
        plugin_id,
        command_name,
        invocation_source,
        runtime_state_us,
        lookup_us,
        instance.load_us,
        context_us,
        run_us,
        total_started.elapsed().as_micros(),
    );
    Ok(PluginCommandExecution { status, outcome })
}

/// The instance one command runs on.
struct CommandInstance {
    loaded: Rc<ProductionPlugin>,
    /// Whether this command retires its fresh command-local instance after
    /// success. Startup and host-owned instances are retired by their owners.
    retire: bool,
    load_us: u128,
}

/// Select the instance for one command. A host-owned frontend keeps one
/// activation for its owner's lifetime; only its owner retires it.
fn command_instance(
    plugin: &bmux_plugin::RegisteredPlugin,
    state: &RuntimeCommandState,
    context: &NativeCommandContext,
    startup_instance: Option<&ProductionPlugin>,
    frontends: Option<&OwnedFrontends>,
    command_local: bool,
) -> Result<CommandInstance> {
    let started = Instant::now();
    let (loaded, retire) = if let Some(instance) = startup_instance {
        (Rc::new(instance.clone()), command_local)
    } else if let Some(hosted) = frontends
        .map(|frontends| frontends.instance(plugin, state, context))
        .transpose()?
        .flatten()
    {
        (Rc::new(hosted), false)
    } else {
        (
            load_command_plugin(
                plugin,
                state,
                command_local.then_some(context.command.as_str()),
            )?,
            command_local,
        )
    };
    Ok(CommandInstance {
        loaded,
        retire,
        load_us: started.elapsed().as_micros(),
    })
}

/// Load the plugin implementation for one command. A command-local activation
/// must never cache a prior command's transport capability, so selected
/// command routes always load a fresh instance; server-owned legacy callers
/// retain their startup cache path.
fn load_command_plugin(
    plugin: &bmux_plugin::RegisteredPlugin,
    state: &RuntimeCommandState,
    cli_command: Option<&str>,
) -> Result<Rc<ProductionPlugin>> {
    if cli_command.is_some() {
        let definitions = standalone_provider_definitions(state)?;
        Ok(Rc::new(load_plugin_with_definitions(
            plugin,
            &plugin_host_metadata(),
            &state.available_capability_providers,
            &state.config,
            &definitions,
            state,
            cli_command,
        )?))
    } else {
        load_cached_plugin(plugin, state)
    }
}

/// Lifecycle context for an instance owned by this selected command runtime.
fn command_lifecycle_context(
    state: &RuntimeCommandState,
    declaration: &bmux_plugin::PluginDeclaration,
) -> NativeLifecycleContext {
    plugin_lifecycle_context(
        &state.config,
        &state.paths,
        declaration,
        PluginLifecycleAvailability {
            services: state.available_services.clone(),
            capabilities: state.available_capability_names.clone(),
            enabled_plugins: state.enabled_plugins.clone(),
            search_roots: state.plugin_search_roots.clone(),
            registered_plugins: state.registered_plugin_infos.clone(),
        },
    )
}

/// Retire a fresh command-local instance after its command succeeded.
fn retire_command_local_plugin(
    state: &RuntimeCommandState,
    plugin: &bmux_plugin::RegisteredPlugin,
    loaded: &ProductionPlugin,
) -> Result<()> {
    let lifecycle = command_lifecycle_context(state, &plugin.declaration);
    // On this blocking command worker the connection owner remains pumping.
    // Never destroy an uncertain/error operation or substitute timeout for
    // settlement. Successful terminal work retires its fresh instance.
    settle_command_cleanup(loaded.deactivate_async(&lifecycle))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_plugin_phase_timing(
    plugin_id: &str,
    command_name: &str,
    invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource,
    runtime_state_us: u128,
    lookup_us: u128,
    load_us: u128,
    context_us: u128,
    run_us: u128,
    total_us: u128,
) {
    let payload = PhasePayload::new("plugin.command")
        .field("plugin_id", plugin_id)
        .field("command_name", command_name)
        .field("invocation_source", format!("{invocation_source:?}"))
        .field("runtime_state_us", runtime_state_us)
        .field("lookup_us", lookup_us)
        .field("load_us", load_us)
        .field("context_us", context_us)
        .field("run_us", run_us)
        .field("total_us", total_us)
        .finish();
    emit_phase_timing(PhaseChannel::Plugin, &payload);
}

/// Definitions for one-command instances (CLI command-local or server-side
/// command bridging), which never own server-lifetime runtime work.
fn standalone_provider_definitions(
    state: &RuntimeCommandState,
) -> Result<BTreeMap<String, bmux_plugin_sdk::definition::PluginDefinition>> {
    #[allow(unused_mut)] // Every entry is feature-gated.
    let mut definitions = BTreeMap::new();
    // Capture the selected invocation's configuration now. Lifecycle polling may
    // run on another thread; factories must not consult command-state TLS.
    #[cfg(feature = "bundled-plugin-recording")]
    definitions.insert(
        "bmux.recording".to_string(),
        recording_binding::definition(
            recording_binding::configuration(&state.config, &state.paths),
            Arc::clone(&HOST_STATE),
            Arc::new(bmux_plugin::EventBus::new()),
        ),
    );
    // The server's lifecycle instance owns the node's consensus runtime.
    #[cfg(feature = "bundled-plugin-cluster")]
    definitions.insert(
        "bmux.cluster".to_string(),
        bmux_cluster_plugin::command_scoped_plugin_definition()
            .map_err(|error| anyhow::anyhow!("cluster command definition invalid: {error}"))?,
    );
    let _ = state;
    Ok(definitions)
}

fn load_cached_plugin(
    plugin: &bmux_plugin::RegisteredPlugin,
    state: &RuntimeCommandState,
) -> Result<Rc<ProductionPlugin>> {
    let plugin_id = plugin.declaration.id.as_str().to_string();
    LOADED_PLUGIN_CACHE.with(|slot| {
        if let Some(existing) = slot.borrow().get(&plugin_id) {
            return Ok(Rc::clone(existing));
        }

        let definitions = standalone_provider_definitions(state)?;
        let loaded = load_plugin_with_definitions(
            plugin,
            &plugin_host_metadata(),
            &state.available_capability_providers,
            &state.config,
            &definitions,
            state,
            None,
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
    let total_started = Instant::now();
    let config_started = Instant::now();
    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let config_us = config_started.elapsed().as_micros();
    let scan_started = Instant::now();
    let registry = scan_available_plugins(&config, &paths)?;
    let scan_us = scan_started.elapsed().as_micros();
    let registry_started = Instant::now();
    let mut command_config = config.clone();
    command_config.plugins.enabled = effective_enabled_plugins(&config, &registry);
    let command_registry = PluginCommandRegistry::build(&command_config, &registry)
        .context("failed building plugin CLI command registry")?;
    let registry_us = registry_started.elapsed().as_micros();
    let resolve_started = Instant::now();
    let resolved = command_registry
        .resolve(args)
        .with_context(|| unknown_external_command_message(args))?;
    let validated_arguments =
        match PluginCommandRegistry::validate_arguments(&resolved.schema, &resolved.arguments) {
            Ok(arguments) => arguments,
            Err(error) => {
                if let Some(error) = error.downcast_ref::<clap::Error>() {
                    error.print()?;
                    return Ok(if error.use_stderr() { 2 } else { 0 });
                }
                return Err(anyhow::anyhow!(format_plugin_argument_validation_error(
                    args, &error
                )));
            }
        };
    let resolve_us = resolve_started.elapsed().as_micros();
    let dispatch_started = Instant::now();
    let status = run_plugin_command(
        &resolved.plugin_id,
        &resolved.command_name,
        &validated_arguments,
    )
    .await?;
    emit_external_plugin_phase_timing(
        args,
        config_us,
        scan_us,
        registry_us,
        resolve_us,
        dispatch_started.elapsed().as_micros(),
        total_started.elapsed().as_micros(),
    );
    Ok(status)
}

fn emit_external_plugin_phase_timing(
    args: &[String],
    config_us: u128,
    scan_us: u128,
    registry_us: u128,
    resolve_us: u128,
    dispatch_us: u128,
    total_us: u128,
) {
    let payload = PhasePayload::new("plugin.external_command")
        .field("command_path", args)
        .field("config_us", config_us)
        .field("scan_us", scan_us)
        .field("registry_us", registry_us)
        .field("resolve_us", resolve_us)
        .field("dispatch_us", dispatch_us)
        .field("total_us", total_us)
        .finish();
    emit_phase_timing(PhaseChannel::Plugin, &payload);
}
#[cfg(test)]
mod tests {
    #[test]
    fn selected_command_snapshot_is_available_before_binding_and_restored_after_nesting() {
        std::thread::spawn(|| {
            let state = RuntimeCommandState {
                runtime_owner: None,
                config: BmuxConfig::default(),
                paths: ConfigPaths::new(
                    "/selected/config".into(),
                    "/selected/runtime".into(),
                    "/selected/data".into(),
                    "/selected/state".into(),
                ),
                registry: Arc::new(PluginRegistry::new()),
                enabled_plugins: Vec::new(),
                available_capability_providers: BTreeMap::new(),
                available_capability_names: Vec::new(),
                available_services: Vec::new(),
                plugin_search_roots: Vec::new(),
                registered_plugin_infos: Vec::new(),
            };
            assert!(selected_command_state().is_err());
            {
                let _outer = SelectedCommandStateGuard::enter(&state);
                assert_eq!(
                    selected_command_state().unwrap().paths.runtime_dir,
                    state.paths.runtime_dir
                );
                let mut nested = state.clone();
                nested.paths.runtime_dir = "/nested/runtime".into();
                {
                    let _inner = SelectedCommandStateGuard::enter(&nested);
                    assert_eq!(
                        selected_command_state().unwrap().paths.runtime_dir,
                        nested.paths.runtime_dir
                    );
                    let _executor = super::super::plugin_kernel::scoped_cli_executor().unwrap();
                }
                assert_eq!(
                    selected_command_state().unwrap().paths.runtime_dir,
                    state.paths.runtime_dir
                );
            }
            assert!(selected_command_state().is_err());
        })
        .join()
        .unwrap();
    }

    #[cfg(feature = "bundled-plugin-recording")]
    #[test]
    fn packaged_definition_preserves_declared_component_roles() {
        let definition = bmux_recording_plugin::plugin_definition();
        definition
            .validate()
            .expect("bundled embedded metadata must validate");
        assert_eq!(
            definition.metadata.roles,
            vec![
                bmux_plugin_sdk::definition::ComponentRole::Backend,
                bmux_plugin_sdk::definition::ComponentRole::Frontend,
            ]
        );
    }

    #[test]
    fn native_bootstrap_pins_explicit_paths_in_worker_local_actions() {
        let root = temp_dir();
        let paths = bmux_config::ConfigPaths::new(
            root.join("config"),
            root.join("runtime"),
            root.join("data"),
            root.join("state"),
        );
        let mut config = bmux_config::BmuxConfig::default();
        config.plugins.search_paths.clear();
        std::thread::spawn(move || {
            initialize_native_runtime_state(config, paths.clone())
                .expect("native explicit bootstrap");
            let state = runtime_command_state().expect("pinned action state");
            assert_eq!(state.paths.config_dir, paths.config_dir);
            assert_eq!(state.paths.runtime_dir, paths.runtime_dir);
            assert_eq!(state.paths.data_dir, paths.data_dir);
            assert_eq!(state.paths.state_dir, paths.state_dir);
            assert_eq!(
                state.config.plugins.search_paths,
                [] as [std::path::PathBuf; 0]
            );
            let callback_paths = paths;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    dispatch_plugin_callback(move || {
                        let callback_state = runtime_command_state()?;
                        assert_eq!(callback_state.paths.config_dir, callback_paths.config_dir);
                        assert_eq!(callback_state.paths.runtime_dir, callback_paths.runtime_dir);
                        Ok(())
                    })
                    .await
                    .expect("callback joined")
                    .expect("callback preserved selected roots");
                });
        })
        .join()
        .expect("worker finished");
        std::fs::remove_dir_all(root).expect("remove owned test directory");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_plugin_callback_does_not_stall_sibling_service_progress() {
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        let (response_tx, response_rx) = std::sync::mpsc::channel();
        let callback = super::dispatch_plugin_callback(move || {
            request_tx.send(()).expect("request sibling service");
            response_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .map_err(anyhow::Error::from)?;
            Ok(())
        });
        let service = async move {
            request_rx.await.expect("callback requested service");
            response_tx.send(()).expect("reply to plugin callback");
        };
        let (dispatch, ()) = tokio::join!(callback, service);
        dispatch
            .expect("blocking task joined")
            .expect("service progressed while callback waited");
    }

    static TEMP_DIR_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time should be monotonic for test")
            .as_nanos();
        let id = TEMP_DIR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("bmux-cli-plugin-test-{nanos}-{id}"));
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

    #[cfg(not(feature = "bundled-plugin-cluster"))]
    #[test]
    fn no_cluster_build_keeps_baseline_plugins_without_cluster_registration() {
        let dirs = static_bundled_workspace_plugin_dirs();
        assert!(!dirs.contains(&"cluster-plugin"));
        crate::runtime::attach::provider::install();
        assert!(matches!(
            crate::runtime::attach::provider::host_registry()
                .resolve(&bmux_client::AttachTarget::parse("cluster://prod/main")),
            Err(bmux_client::AttachProviderResolutionError::NoProvider { .. })
        ));
    }

    #[test]
    fn workspace_plugin_discovery_is_debug_only() {
        assert_eq!(
            workspace_bundled_plugin_root().is_some(),
            cfg!(debug_assertions)
        );
    }

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
    fn source_configuration_round_trips_and_rejects_unknown_policy() {
        let config: BmuxConfig = toml::from_str(
            "[plugins.sources.'example.plugin']\nsource='dynamic'\nmanifest='plugins/example/plugin.toml'\n",
        ).unwrap();
        assert_eq!(
            config.plugins.sources.get("example.plugin"),
            Some(&PluginSourceConfig::Dynamic {
                manifest: PathBuf::from("plugins/example/plugin.toml"),
            })
        );
        let encoded = toml::to_string(&config).unwrap();
        let decoded: BmuxConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.plugins.sources, config.plugins.sources);
        assert!(
            toml::from_str::<BmuxConfig>(
                "[plugins.sources.'example.plugin']\nsource='automatic_fallback'\n"
            )
            .is_err()
        );
        assert!(
            toml::from_str::<BmuxConfig>(
                "[plugins.sources.'example.plugin']\nsource='bundled'\nmanifest='ignored.toml'\n"
            )
            .is_err()
        );
    }

    #[test]
    fn dynamic_selection_suppresses_compiled_bundled_registration() {
        let mut bundled = PluginRegistry::new();
        register_static_bundled_plugins(&mut bundled);
        let sources = bundled
            .iter()
            .map(|plugin| {
                (
                    plugin.declaration.id.as_str().to_string(),
                    PluginSourceConfig::Dynamic {
                        manifest: PathBuf::from("selected.toml"),
                    },
                )
            })
            .collect();
        let mut selected = PluginRegistry::new();
        register_selected_bundled_plugins(&mut selected, &sources).unwrap();
        assert_eq!(selected.iter().count(), 0);
    }

    #[test]
    fn explicit_dynamic_source_requires_matching_logical_identity() {
        let dir = temp_dir();
        let manifest = dir.join("plugin.toml");
        fs::write(
            &manifest,
            "id='actual.plugin'\nname='Actual'\nversion='0.1.0'\nentry='native.dylib'\n",
        )
        .unwrap();
        let mut config = BmuxConfig::default();
        config.plugins.sources.insert(
            "expected.plugin".into(),
            PluginSourceConfig::Dynamic { manifest },
        );
        let mut registry = PluginRegistry::new();
        let error = register_selected_dynamic_plugins(&mut registry, &config).unwrap_err();
        assert!(format!("{error:#}").contains("expected 'expected.plugin'"));
        assert_eq!(registry.iter().count(), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explicit_dynamic_source_never_falls_back_when_entry_is_missing() {
        let dir = temp_dir();
        let manifest = dir.join("plugin.toml");
        fs::write(
            &manifest,
            "id='example.plugin'\nname='Example'\nversion='0.1.0'\nentry='missing.dylib'\n",
        )
        .unwrap();
        let mut config = BmuxConfig::default();
        config.plugins.sources.insert(
            "example.plugin".into(),
            PluginSourceConfig::Dynamic { manifest },
        );
        let error =
            register_selected_dynamic_plugins(&mut PluginRegistry::new(), &config).unwrap_err();
        assert!(error.to_string().contains("entry is not a file"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explicit_dynamic_source_preserves_defaults_and_disable_precedence() {
        let dir = temp_dir();
        let manifest = dir.join("plugin.toml");
        fs::write(&manifest, "id='example.plugin'\nname='Example'\nversion='0.1.0'\nentry='native.dylib'\nenabled_by_default=true\n").unwrap();
        // Registration only: this is not a loadable native artifact fixture.
        fs::write(dir.join("native.dylib"), []).unwrap();
        let mut config = BmuxConfig::default();
        config.plugins.sources.insert(
            "example.plugin".into(),
            PluginSourceConfig::Dynamic { manifest },
        );
        let mut registry = PluginRegistry::new();
        register_selected_dynamic_plugins(&mut registry, &config).unwrap();
        let selected = registry.get("example.plugin").unwrap();
        assert!(!selected.bundled_static);
        assert_eq!(selected.search_root, fs::canonicalize(&dir).unwrap());
        assert_eq!(
            effective_enabled_plugins(&config, &registry),
            vec!["example.plugin"]
        );
        config.plugins.disabled.push("example.plugin".into());
        assert_eq!(
            effective_enabled_plugins(&config, &registry),
            Vec::<String>::new()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explicit_bundled_source_rejects_unavailable_implementation() {
        let mut config = BmuxConfig::default();
        config
            .plugins
            .sources
            .insert("not.compiled".into(), PluginSourceConfig::Bundled);
        let error =
            register_selected_dynamic_plugins(&mut PluginRegistry::new(), &config).unwrap_err();
        assert!(format!("{error:#}").contains("no candidates for 'not.compiled'"));
    }

    #[test]
    fn empty_source_selection_keeps_no_plugin_registry_empty() {
        let mut registry = PluginRegistry::new();
        let config = BmuxConfig::default();
        register_selected_dynamic_plugins(&mut registry, &config).unwrap();
        assert_eq!(registry.iter().count(), 0);
        assert_eq!(
            effective_enabled_plugins(&config, &registry),
            Vec::<String>::new()
        );
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
    fn bundled_opt_in_plugin_requires_explicit_enablement() {
        let mut registry = PluginRegistry::new();
        registry
            .register_bundled_manifest(
                r#"
id = "test.opt_in"
name = "Opt In"
version = "0.1.0"
enabled_by_default = false
"#,
            )
            .expect("opt-in bundled plugin should register");

        let mut config = BmuxConfig::default();
        assert!(
            !effective_enabled_plugins(&config, &registry).contains(&"test.opt_in".to_string())
        );

        config.plugins.enabled.push("test.opt_in".to_string());
        assert!(effective_enabled_plugins(&config, &registry).contains(&"test.opt_in".to_string()));
    }

    #[test]
    fn presentation_defaults_to_bottom_tab_bar_without_sidebar() {
        let mut registry = PluginRegistry::new();
        register_static_bundled_plugins(&mut registry);

        let enabled = effective_enabled_plugins(&BmuxConfig::default(), &registry);
        assert!(enabled.iter().any(|plugin| plugin == "bmux.tab_bar"));
        assert!(!enabled.iter().any(|plugin| plugin == "bmux.sidebar"));
    }

    #[test]
    fn presentation_plugins_support_all_enablement_combinations() {
        let mut registry = PluginRegistry::new();
        register_static_bundled_plugins(&mut registry);
        let combinations = [(true, true), (true, false), (false, true), (false, false)];
        for (tab_bar, sidebar) in combinations {
            let mut config = BmuxConfig::default();
            if tab_bar {
                config.plugins.enabled.push("bmux.tab_bar".to_string());
            } else {
                config.plugins.disabled.push("bmux.tab_bar".to_string());
            }
            if sidebar {
                config.plugins.enabled.push("bmux.sidebar".to_string());
            } else {
                config.plugins.disabled.push("bmux.sidebar".to_string());
            }
            let enabled = effective_enabled_plugins(&config, &registry);
            assert_eq!(
                enabled.iter().any(|plugin| plugin == "bmux.tab_bar"),
                tab_bar
            );
            assert_eq!(
                enabled.iter().any(|plugin| plugin == "bmux.sidebar"),
                sidebar
            );
        }
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
                plugin_manifest("bmux.tabs", "windows.dylib"),
            )
            .expect("bundled plugin should register");

        let config = BmuxConfig::default();
        let enabled = effective_enabled_plugins(&config, &registry);
        assert!(enabled.iter().any(|plugin_id| plugin_id == "bmux.tabs"));
    }

    #[test]
    fn windows_manifest_marks_navigation_commands_repeat_safe() {
        let mut registry = PluginRegistry::new();
        registry
            .register_bundled_manifest(include_str!("../../../../plugins/tabs-plugin/plugin.toml"))
            .expect("windows manifest should register");

        assert!(registry.command_accepts_repeat("bmux.tabs", "focus-pane-in-direction"));
        assert!(registry.command_accepts_repeat("bmux.tabs", "resize-pane"));
        assert!(registry.command_accepts_repeat("bmux.tabs", "next-tab"));
        assert!(registry.command_accepts_repeat("bmux.tabs", "prev-tab"));
        assert!(!registry.command_accepts_repeat("bmux.tabs", "close-active-pane"));
    }

    #[test]
    fn backend_deployment_retains_frontend_inventory_but_does_not_activate_it() {
        let mut registry = PluginRegistry::new();
        registry.register_bundled_manifest(
            "id='example.frontend'\nname='Frontend'\nversion='1.0.0'\ncomponent_roles=['frontend']\n",
        ).unwrap();
        let mut config = BmuxConfig::default();
        assert_eq!(
            backend_enabled_plugins(&config, &registry).unwrap(),
            Vec::<String>::new()
        );
        assert!(registry.get("example.frontend").is_some());
        config.plugins.enabled.push("example.frontend".into());
        assert!(
            backend_enabled_plugins(&config, &registry)
                .unwrap_err()
                .to_string()
                .contains("does not export Backend")
        );
    }

    #[test]
    fn deployed_frontend_role_is_selected_without_backend_retry() {
        use bmux_plugin_sdk::definition::ComponentRole;
        let manifest = PluginManifest::from_toml_str(
            "id='example.frontend'\nname='Frontend'\nversion='1.0.0'\ncomponent_roles=['frontend']\n",
        ).unwrap();
        assert_eq!(
            deployed_component_role(&manifest).unwrap(),
            ComponentRole::Frontend
        );
        let mut inventory = bmux_plugin::PluginCandidateInventory::new();
        inventory.add_bundled_manifest(
            "id='example.frontend'\nname='Frontend'\nversion='1.0.0'\ncomponent_roles=['frontend']\n"
        ).unwrap();
        assert_eq!(
            inventory_component_role(
                &inventory,
                "example.frontend",
                &bmux_plugin::PluginCandidatePreference::Bundled
            )
            .unwrap(),
            ComponentRole::Frontend
        );
    }

    fn dispatch_probe_definition(
        plugin_id: &str,
        manifest: &str,
    ) -> (
        bmux_plugin_sdk::definition::PluginDefinition,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use bmux_plugin_sdk::definition::{
            CommandOutcome, ComponentRole, PluginCallContext, PluginDefinition, PluginFuture,
            PluginInstance, PluginMetadata,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct DispatchProbe(
            Arc<AtomicUsize>,
            bmux_plugin_sdk::definition::LoadedInstanceId,
        );
        impl PluginInstance for DispatchProbe {
            fn command(
                &self,
                call: PluginCallContext,
                context: NativeCommandContext,
            ) -> PluginFuture<'_, CommandOutcome> {
                let probe = |port: &bmux_plugin_sdk::definition::PluginHostServices| {
                    port.invoke(bmux_plugin_sdk::HostServiceRequest {
                        capability: "example.undeclared",
                        kind: bmux_plugin_sdk::ServiceKind::Query,
                        interface: "example.undeclared/v1",
                        operation: "probe",
                        payload: &[],
                    })
                    .unwrap_err()
                    .to_string()
                };
                let execution = call
                    .callback_execution
                    .clone()
                    .expect("selected command must carry callback authority");
                let ports = [call.host_services, context.host_service_port]
                    .map(|port| port.expect("selected command must have owned host services"));
                // Outside the author entry a call takes this command's own
                // selected dispatch (declaration, capability checks and
                // authority), never an ambient fallback: the undeclared
                // capability is refused there.
                for port in &ports {
                    let error = probe(port);
                    assert!(
                        error.contains("capability not declared by caller"),
                        "{error}"
                    );
                }
                let entry = execution
                    .enter_adapter_entry(self.1)
                    .expect("selected command should enter its callback authority");
                // Both surfaces must carry the selected command's owned port,
                // not an unbound loader port or ambient host-service fallback.
                for port in &ports {
                    let error = probe(port);
                    assert!(error.contains("not declared by caller"), "{error}");
                }
                drop(entry);
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    CommandOutcome {
                        exit_code: 23,
                        error: None,
                        metadata: std::collections::BTreeMap::new(),
                    }
                })
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let factory_calls = Arc::clone(&calls);
        let definition = PluginDefinition::new(
            PluginMetadata {
                plugin_id: plugin_id.to_owned(),
                version: "1.0.0".into(),
                roles: vec![ComponentRole::Backend],
                required_capabilities: Vec::new(),
                contracts: Vec::new(),
                manifest_toml: Some(manifest.to_owned()),
            },
            move |scope: bmux_plugin_sdk::definition::PluginCreateContext| {
                Ok(
                    Arc::new(DispatchProbe(Arc::clone(&factory_calls), scope.instance))
                        as Arc<dyn PluginInstance>,
                )
            },
        );
        (definition, calls)
    }

    #[test]
    fn static_bundled_load_uses_selected_definition_and_dispatches_command() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let driver = crate::runtime_owner::RuntimeDriver::new(runtime).unwrap();
        let owner = driver.owner();
        owner.handle().block_on(async {
        use std::sync::atomic::Ordering;
        let plugin_id = format!("example.selected-{}", Uuid::new_v4());
        let manifest = format!(
            "id='{plugin_id}'\nname='Dispatch probe'\nversion='1.0.0'\ncomponent_roles=['backend']\n"
        );
        let (definition, calls) = dispatch_probe_definition(&plugin_id, &manifest);
        let embedded = register_bundled_definition(&plugin_id, definition)
            .expect("selected definition should register");
        assert!(
            bmux_plugin::NativePluginLoader::registered_definition(&plugin_id)
                .expect("definition lookup should succeed")
                .is_some()
        );
        let mut registry = PluginRegistry::new();
        registry
            .register_bundled_manifest(&embedded)
            .expect("manifest should register");
        let plugin = registry
            .get(&plugin_id)
            .expect("probe should be registered");
        assert!(plugin.bundled_static);
        let config = BmuxConfig::default();
        let root = temp_dir();
        let paths = ConfigPaths::new(
            root.join("config"),
            root.join("runtime"),
            root.join("data"),
            root.join("state"),
        );
        // The owner is carried explicitly in the state, not entered as context.
        let command_state = build_runtime_command_state(
            config.clone(),
            paths.clone(),
            Arc::new(registry.clone()),
            Some(&owner),
        )
        .expect("isolated selected command runtime should resolve");
        let _selected_runtime = SelectedCommandStateGuard::enter(&command_state);
        let loaded = load_plugin(plugin, &plugin_host_metadata(), &BTreeMap::new(), &config)
            .expect("selected bundled definition should load");
        assert_eq!(loaded.declaration.id.as_str(), plugin_id);
        let context = plugin_command_context(
            &config,
            &paths,
            &plugin.declaration,
            "probe",
            &[],
            Vec::new(),
            Vec::new(),
            vec![plugin_id.clone()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
            bmux_plugin_sdk::NativeCommandInvocationSource::Cli,
        );
        let (status, outcome) = loaded
            .run_command_with_context_and_outcome("probe", &[], Some(&context))
            .expect("shared execution should activate, acknowledge and dispatch");
        assert_eq!(status, 23);
        assert_eq!(outcome.error_message, None);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "selected implementation must execute exactly once"
        );
        let lifecycle = plugin_lifecycle_context(
            &config,
            &paths,
            &plugin.declaration,
            PluginLifecycleAvailability {
                services: Vec::new(),
                capabilities: Vec::new(),
                enabled_plugins: vec![plugin.manifest.id.clone()],
                search_roots: Vec::new(),
                registered_plugins: Vec::new(),
            },
        );
        loaded
            .deactivate_async(&lifecycle)
            .await
            .expect("shared execution should deactivate and destroy");
        let command = run_command_on_selected_target(
            &command_state, &plugin_id, "probe", &[],
            Err("test has no selected reachable daemon".into()),
        ).await.expect("host-independent selected command must not connect");
        assert_eq!(command.status, 23);
        assert_eq!(calls.load(Ordering::SeqCst), 2,
            "actual standalone worker must execute the selected author exactly once");
        fs::remove_dir_all(root).expect("remove owned test directory");
        });
        drop(owner);
        driver.shutdown().unwrap();
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
                plugin_manifest("bmux.tabs", "windows.dylib"),
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
        assert!(enabled.iter().any(|plugin_id| plugin_id == "bmux.tabs"));
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
                plugin_manifest("bmux.tabs", "windows.dylib"),
            )
            .expect("bundled plugin should register");

        let mut config = BmuxConfig::default();
        config.plugins.disabled.push("bmux.tabs".to_string());
        let enabled = effective_enabled_plugins(&config, &registry);
        assert!(!enabled.iter().any(|plugin_id| plugin_id == "bmux.tabs"));
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
                plugin_manifest("bmux.tabs", "windows.dylib"),
            )
            .expect("bundled plugin should register");

        let config = BmuxConfig::default();
        let enabled = effective_enabled_plugins(&config, &registry);
        assert!(!enabled.iter().any(|plugin_id| plugin_id == "bmux.tabs"));
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
            None,
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
            None,
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
                assert_eq!(arguments, [] as [std::string::String; 0]);
            }
            other => panic!("expected plugin runtime parse, got {other:?}"),
        }
    }

    #[test]
    fn interactive_record_cli_preserves_literal_argv_and_supports_help() {
        let dir = temp_dir();
        let manifest = dir.join("plugin.toml");
        fs::write(
            &manifest,
            include_str!("../../../../plugins/recording-plugin/plugin.toml"),
        )
        .unwrap();
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(
                &manifest,
                PluginManifest::from_toml_str(&format!(
                    "entry = 'recording.dylib'\n{}",
                    include_str!("../../../../plugins/recording-plugin/plugin.toml")
                ))
                .unwrap(),
            )
            .unwrap();
        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("bmux.recording".into());
        for prefix in [vec!["bmux"], vec!["bmux", "--log-level", "debug"]] {
            let mut argv: Vec<OsString> = prefix.into_iter().map(OsString::from).collect();
            argv.extend(
                ["record", "--json", "--", "lazygit", "--help", "a b", "--"]
                    .into_iter()
                    .map(OsString::from),
            );
            assert_recording_plugin_cli_parse(
                &config,
                &registry,
                &argv,
                "record",
                &["--json", "--", "lazygit", "--help", "a b", "--"],
            );
        }
        let argv = ["bmux", "record", "--help"].map(OsString::from);
        let parsed = parse_runtime_cli_with_registry(
            &argv,
            &config,
            &registry,
            None,
            bmux_config::ConfigLoadOverrides::default(),
        )
        .unwrap();
        match parsed {
            ParsedRuntimeCli::ImmediateExit {
                code,
                output,
                stderr,
            } => {
                assert_eq!(code, 0);
                assert!(!stderr);
                for option in ["--output", "--export", "--gif", "--json"] {
                    assert!(output.contains(option), "missing {option}: {output}");
                }
            }
            other => panic!("expected help, got {other:?}"),
        }
    }

    fn recording_plugin_registry_for_cli_tests() -> (BmuxConfig, PluginRegistry) {
        let dir = temp_dir();
        let plugin_dir = dir.join("recording");
        fs::create_dir_all(&plugin_dir).expect("plugin dir should exist");
        fs::write(plugin_dir.join("recording.dylib"), []).expect("entry should be written");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(
                &plugin_dir.join("plugin.toml"),
                plugin_manifest_with_commands(
                    "bmux.recording",
                    "recording.dylib",
                    "owns_namespaces=['recording']
owns_paths=[['server','recording'],['playbook','from-recording']]
[[commands]]
name='recording-cut'
path=['recording','cut']
summary='cut'
execution='provider_exec'
expose_in_cli=true
[[commands.arguments]]
name='args'
kind='string'
position=0
multiple=true
trailing_var_arg=true
allow_hyphen_values=true
[[commands]]
name='server-recording'
path=['server','recording']
summary='rolling'
execution='provider_exec'
expose_in_cli=true
[[commands.arguments]]
name='args'
kind='string'
position=0
multiple=true
trailing_var_arg=true
allow_hyphen_values=true
[[commands]]
name='playbook-from-recording'
path=['playbook','from-recording']
summary='from recording'
execution='provider_exec'
expose_in_cli=true
[[commands.arguments]]
name='args'
kind='string'
position=0
multiple=true
trailing_var_arg=true
allow_hyphen_values=true
",
                ),
            )
            .expect("plugin should register");

        let mut config = BmuxConfig::default();
        config.plugins.enabled.push("bmux.recording".to_string());
        (config, registry)
    }

    fn assert_recording_plugin_cli_parse(
        config: &BmuxConfig,
        registry: &PluginRegistry,
        argv: &[OsString],
        expected_command: &str,
        expected_args: &[&str],
    ) {
        let parsed = parse_runtime_cli_with_registry(
            argv,
            config,
            registry,
            None,
            bmux_config::ConfigLoadOverrides::default(),
        )
        .expect("runtime CLI should parse plugin-owned recording command");
        match parsed {
            ParsedRuntimeCli::Plugin {
                plugin_id,
                command_name,
                arguments,
                ..
            } => {
                assert_eq!(plugin_id, "bmux.recording");
                assert_eq!(command_name, expected_command);
                assert_eq!(arguments, expected_args);
            }
            other => panic!("expected plugin runtime parse, got {other:?}"),
        }
    }

    #[test]
    fn runtime_cli_prefers_recording_plugin_namespace_over_static_builtin() {
        let (config, registry) = recording_plugin_registry_for_cli_tests();
        assert_recording_plugin_cli_parse(
            &config,
            &registry,
            &[
                OsString::from("bmux"),
                OsString::from("recording"),
                OsString::from("cut"),
                OsString::from("--last-seconds"),
                OsString::from("5"),
                OsString::from("--name"),
                OsString::from("clip"),
            ],
            "recording-cut",
            &["--last-seconds", "5", "--name", "clip"],
        );
        assert_recording_plugin_cli_parse(
            &config,
            &registry,
            &[
                OsString::from("bmux"),
                OsString::from("server"),
                OsString::from("recording"),
                OsString::from("status"),
                OsString::from("--json"),
            ],
            "server-recording",
            &["status", "--json"],
        );
        assert_recording_plugin_cli_parse(
            &config,
            &registry,
            &[
                OsString::from("bmux"),
                OsString::from("playbook"),
                OsString::from("from-recording"),
                OsString::from("abc123"),
                OsString::from("--output"),
                OsString::from("out.dsl"),
            ],
            "playbook-from-recording",
            &["abc123", "--output", "out.dsl"],
        );
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
                        "bmux.tabs",
                        "windows.dylib",
                        "owns_namespaces=['new-tab']\n[[commands]]\nname='new-tab'\npath=['new-tab']\nsummary='new'\nexecution='provider_exec'\nexpose_in_cli=true\n",
                    ),
                )
                .expect("plugin should register");

        let config = BmuxConfig::default();
        let argv = vec![OsString::from("bmux"), OsString::from("new-tab")];
        let parsed = parse_runtime_cli_with_registry(
            &argv,
            &config,
            &registry,
            None,
            bmux_config::ConfigLoadOverrides::default(),
        )
        .expect("runtime CLI should parse bundled plugin command");
        match parsed {
            ParsedRuntimeCli::Plugin { plugin_id, .. } => {
                assert_eq!(plugin_id, "bmux.tabs");
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
            None,
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
                        ..
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
            entrypoint: bmux_plugin::PluginEntrypoint::Native,
            description: None,
            homepage: None,
            provider_priority: 0,
            execution_class: bmux_plugin::PluginExecutionClass::NativeStandard,
            concurrency: bmux_plugin::PluginConcurrencyConfig::Concurrent,
            owns_namespaces: std::collections::BTreeSet::new(),
            owns_paths: std::collections::BTreeSet::new(),
            optional_capabilities: std::collections::BTreeSet::new(),
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
                concurrency: None,
                reentrant_operations: std::collections::BTreeSet::new(),
            }],
            commands: Vec::new(),
            event_subscriptions: Vec::new(),
            event_publications: Vec::new(),
            dependencies: Vec::new(),
            lifecycle: bmux_plugin::PluginLifecycle::default(),
            ready_signals: Vec::new(),
        };
        let context = plugin_lifecycle_context(
            &config,
            &paths,
            &declaration,
            PluginLifecycleAvailability {
                services: service_descriptors_from_declarations([&declaration]),
                capabilities: vec![
                    "bmux.commands".to_string(),
                    "example.provider.write".to_string(),
                ],
                enabled_plugins: vec!["example.plugin".to_string()],
                search_roots: vec!["/plugins".to_string()],
                registered_plugins: Vec::new(),
            },
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
        assert_eq!(context.services.len(), 8);
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
                .any(|service| service.interface_id == "volatile-state-query/v1")
        );
        assert!(
            context
                .services
                .iter()
                .any(|service| service.interface_id == "volatile-state-command/v1")
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
            entrypoint: bmux_plugin::PluginEntrypoint::Native,
            description: None,
            homepage: None,
            provider_priority: 0,
            execution_class: bmux_plugin::PluginExecutionClass::NativeStandard,
            concurrency: bmux_plugin::PluginConcurrencyConfig::Concurrent,
            owns_namespaces: std::collections::BTreeSet::new(),
            owns_paths: std::collections::BTreeSet::new(),
            optional_capabilities: std::collections::BTreeSet::new(),
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
                    concurrency: None,
                    reentrant_operations: std::collections::BTreeSet::new(),
                },
                bmux_plugin_sdk::PluginService {
                    capability: bmux_plugin_sdk::HostScope::new("example.provider.write")
                        .expect("capability should parse"),
                    kind: bmux_plugin_sdk::ServiceKind::Command,
                    interface_id: "provider-command/v1".to_string(),
                    concurrency: None,
                    reentrant_operations: std::collections::BTreeSet::new(),
                },
            ],
            commands: Vec::new(),
            event_subscriptions: Vec::new(),
            event_publications: Vec::new(),
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
            Vec::new(),
            None,
            bmux_plugin_sdk::NativeCommandInvocationSource::Unknown,
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
        assert_eq!(context.services.len(), 9);
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
        let server_event = bmux_client::ServerEvent::ServerStarted;
        let event =
            plugin_event_from_server_event(&server_event).expect("plugin event should build");
        assert_eq!(event.kind.as_str(), "bmux.core/server_started");
        assert_eq!(
            event.payload,
            serde_json::to_value(server_event).expect("server event should encode")
        );
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
            Some(&"plugin:bmux.tabs:split-pane --direction vertical".to_string())
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
        let message = format_plugin_command_run_error("bmux.tabs", "kill", &error);
        assert!(message.contains("failed running plugin command 'bmux.tabs:kill'"));
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
            &["bmux.tabs".to_string(), "bmux.permissions".to_string()],
        );
        assert!(message.contains("plugin 'missing.plugin' was not found"));
        assert!(message.contains("bmux.tabs, bmux.permissions"));
    }

    #[test]
    fn format_plugin_not_found_message_handles_empty_registry() {
        let empty: [&str; 0] = [];
        let message = format_plugin_not_found_message("missing.plugin", &empty);
        assert_eq!(message, "plugin 'missing.plugin' was not found");
    }

    #[test]
    fn format_plugin_not_enabled_message_points_to_plugins_enabled() {
        let message = format_plugin_not_enabled_message("bmux.tabs");
        assert!(message.contains("plugin 'bmux.tabs' is not enabled"));
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
