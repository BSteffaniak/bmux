#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![cfg_attr(feature = "static-bundled", allow(dead_code))]

use bmux_appearance::{
    RUNTIME_APPEARANCE_STATE_KIND, RuntimeAppearance, RuntimeAppearancePatch,
    RuntimeBorderAppearancePatch, RuntimeContentBlendPatch, RuntimeContentEffectBgPredicate,
    RuntimeContentEffectPatch, RuntimeContentEffectScope, RuntimeStatusAppearancePatch,
};
use bmux_ipc::Request as IpcRequest;
use bmux_performance_plugin_api::{capabilities, performance_commands, performance_state};
use bmux_plugin::prompt;
use bmux_plugin::{HostRuntimeApi, ServiceCaller};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    HostConnectionInfo, NativeServiceContext, PluginEvent, PromptEvent, PromptResponse,
    PromptValue, ServiceKind, ServiceResponse, StorageGetRequest, StorageSetRequest,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tracing::{info, warn};

const STORAGE_SELECTED_APPEARANCE: &str = "selected_theme";
const STORAGE_PERFORMANCE_THEME_SETTINGS: &str = "theme_settings.performance";

#[derive(Default)]
pub struct ThemePlugin {
    lifecycle_context: Option<NativeLifecycleContext>,
}

impl RustPlugin for ThemePlugin {
    type Contract = bmux_plugin_sdk::NoPluginContract;

    fn activate(&mut self, context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        info!(
            data_dir = %context.connection.data_dir,
            config_dirs = ?context.connection.config_dir_candidate_paths(),
            settings_present = context.settings.is_some(),
            "theme plugin activating",
        );
        self.lifecycle_context = Some(context.clone());
        apply_configured_appearance(&context);
        apply_configured_theme_extensions(&context);
        Ok(EXIT_OK)
    }

    fn handle_event(&mut self, event: PluginEvent) -> Result<i32, PluginCommandError> {
        if event.kind.as_str() == "bmux.core/server_started"
            && let Some(context) = self.lifecycle_context.as_ref()
        {
            info!(
                data_dir = %context.connection.data_dir,
                event_kind = %event.kind.as_str(),
                "theme plugin handling lifecycle event; reapplying theme state",
            );
            apply_configured_appearance(context);
            apply_configured_theme_extensions(context);
        }
        Ok(EXIT_OK)
    }

    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        bmux_plugin_sdk::route_command!(context, {
            "pick-theme" => pick_theme(&context),
        })
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "theme-state", "active-appearance" => |_req: (), ctx| {
                let appearance = active_runtime_appearance(ctx).ok_or_else(|| {
                    ServiceResponse::error("theme_not_found", "active theme was not found")
                })?;
                info!("active runtime appearance service returned resolved theme appearance");
                Ok(appearance)
            },
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ThemeConfig {
    name: String,
    foreground: Option<String>,
    background: Option<String>,
    cursor: Option<String>,
    selection_background: Option<String>,
    border: BorderColors,
    status: StatusColors,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    content_effects: BTreeMap<String, ThemeContentEffect>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    modes: BTreeMap<String, Self>,
    #[serde(rename = "plugins", skip_serializing_if = "BTreeMap::is_empty")]
    plugins: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct BorderColors {
    active: Option<String>,
    inactive: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct StatusColors {
    background: Option<String>,
    foreground: Option<String>,
    active_window: Option<String>,
    mode_indicator: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ThemeContentEffect {
    enabled: Option<bool>,
    scope: Option<RuntimeContentEffectScope>,
    when_bg: Option<RuntimeContentEffectBgPredicate>,
    background_blend: ThemeContentBlend,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ThemeContentBlend {
    color: Option<String>,
    amount: Option<f32>,
    amount_permille: Option<u16>,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            foreground: None,
            background: None,
            cursor: None,
            selection_background: None,
            border: BorderColors::default(),
            status: StatusColors::default(),
            content_effects: BTreeMap::new(),
            modes: BTreeMap::new(),
            plugins: BTreeMap::new(),
        }
    }
}

impl From<&ThemeConfig> for RuntimeAppearancePatch {
    fn from(theme: &ThemeConfig) -> Self {
        Self {
            foreground: theme.foreground.clone(),
            background: theme.background.clone(),
            cursor: theme.cursor.clone(),
            selection_background: theme.selection_background.clone(),
            border: RuntimeBorderAppearancePatch {
                active: theme.border.active.clone(),
                inactive: theme.border.inactive.clone(),
            },
            status: RuntimeStatusAppearancePatch {
                background: theme.status.background.clone(),
                foreground: theme.status.foreground.clone(),
                active_window: theme.status.active_window.clone(),
                mode_indicator: theme.status.mode_indicator.clone(),
            },
            content_effects: theme
                .content_effects
                .iter()
                .map(|(name, effect)| (name.clone(), RuntimeContentEffectPatch::from(effect)))
                .collect(),
        }
    }
}

impl From<&ThemeContentEffect> for RuntimeContentEffectPatch {
    fn from(effect: &ThemeContentEffect) -> Self {
        Self {
            enabled: effect.enabled,
            scope: effect.scope,
            when_bg: effect.when_bg,
            background_blend: RuntimeContentBlendPatch::from(&effect.background_blend),
        }
    }
}

impl From<&ThemeContentBlend> for RuntimeContentBlendPatch {
    fn from(blend: &ThemeContentBlend) -> Self {
        Self {
            color: blend.color.clone(),
            amount_permille: blend
                .amount_permille
                .or_else(|| blend.amount.map(amount_to_permille)),
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Value is clamped to 0..=1000 before casting.
fn amount_to_permille(amount: f32) -> u16 {
    (amount.clamp(0.0, 1.0) * 1000.0).round() as u16
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum ThemePersistence {
    #[default]
    DeclaredOnConnect,
    PersistBetweenConnects,
}

#[derive(Debug, Default, Deserialize)]
struct ThemePluginSettings {
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    themes: Vec<String>,
    #[serde(default)]
    persistence: ThemePersistence,
    #[serde(default)]
    components: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone)]
struct ThemeCatalogEntry {
    name: String,
    theme: ThemeConfig,
}

#[derive(Debug, Clone)]
struct ResolvedTheme {
    appearance: RuntimeAppearance,
    plugins: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveThemeSource {
    Declared,
    DeclaredStack,
    Persisted,
    DefaultFallback,
}

#[derive(Debug, Clone)]
struct ActiveThemeStack {
    stack: Vec<String>,
    source: ActiveThemeSource,
    requested_name: Option<String>,
}

#[derive(Debug, Clone)]
struct ActiveThemeResolution {
    stack: Vec<String>,
    source: ActiveThemeSource,
    requested_name: Option<String>,
    theme: ResolvedTheme,
}

#[derive(Debug, Serialize, Deserialize)]
struct ApplyThemeExtensionArgs {
    toml: String,
    config_dir_candidates: Vec<String>,
}

trait ThemeHostContext: HostRuntimeApi {
    fn settings_value(&self) -> Option<&toml::Value>;

    fn connection_info(&self) -> &HostConnectionInfo;
}

impl ThemeHostContext for NativeLifecycleContext {
    fn settings_value(&self) -> Option<&toml::Value> {
        self.settings.as_ref()
    }

    fn connection_info(&self) -> &HostConnectionInfo {
        &self.connection
    }
}

impl ThemeHostContext for NativeCommandContext {
    fn settings_value(&self) -> Option<&toml::Value> {
        self.settings.as_ref()
    }

    fn connection_info(&self) -> &HostConnectionInfo {
        &self.connection
    }
}

impl ThemeHostContext for NativeServiceContext {
    fn settings_value(&self) -> Option<&toml::Value> {
        self.settings.as_ref()
    }

    fn connection_info(&self) -> &HostConnectionInfo {
        &self.connection
    }
}

fn pick_theme(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let handle = tokio::runtime::Handle::try_current().map_err(|_| {
        PluginCommandError::unavailable("no tokio runtime available; theme picker requires attach")
    })?;
    handle.spawn(run_theme_picker(context.clone()));
    Ok(EXIT_OK)
}

fn apply_configured_appearance(context: &NativeLifecycleContext) {
    if let Some(active) = configured_theme(context) {
        log_active_theme(context, &active);
        publish_runtime_appearance(&active.theme);
        info!(
            source = ?active.source,
            requested_name = active.requested_name.as_deref().unwrap_or(""),
            stack = ?active.stack,
            "runtime appearance published for active theme",
        );
    } else {
        warn!("no active theme resolved for runtime appearance apply");
    }
}

fn apply_configured_theme_extensions(context: &NativeLifecycleContext) {
    let Some(active) = configured_theme(context) else {
        warn!("no active theme resolved for startup extension apply");
        return;
    };
    let catalog = load_theme_catalog(&context.connection.config_dir_candidate_paths());
    let all_plugin_ids = theme_catalog_plugin_ids(&catalog);
    info!(
        source = ?active.source,
        requested_name = active.requested_name.as_deref().unwrap_or(""),
        stack = ?active.stack,
        extension_plugin_count = all_plugin_ids.len(),
        "applying theme extension settings",
    );
    apply_theme_extensions(
        context,
        &active.theme,
        &all_plugin_ids,
        &context.connection.config_dir_candidates,
    );
    if active.stack.iter().any(|name| name == "performance") {
        apply_persisted_performance_theme_settings(context);
    }
}

fn active_runtime_appearance(
    context: &(impl ThemeHostContext + ?Sized),
) -> Option<RuntimeAppearance> {
    configured_theme(context).map(|active| active.theme.appearance)
}

fn configured_theme(context: &(impl ThemeHostContext + ?Sized)) -> Option<ActiveThemeResolution> {
    let settings = parse_settings(context.settings_value());
    let catalog = load_theme_catalog(&context.connection_info().config_dir_candidate_paths());
    info!(
        data_dir = %context.connection_info().data_dir,
        catalog_count = catalog.len(),
        configured_theme = settings.theme.as_deref().unwrap_or(""),
        configured_stack = ?settings.themes,
        persistence = ?settings.persistence,
        "theme settings parsed",
    );
    let active = active_theme_stack(context, &settings, &catalog);
    let theme = resolve_theme_stack_with_settings(&catalog, &active.stack, &settings)?;
    Some(ActiveThemeResolution {
        stack: active.stack,
        source: active.source,
        requested_name: active.requested_name,
        theme,
    })
}

fn log_active_theme(context: &impl ThemeHostContext, active: &ActiveThemeResolution) {
    info!(
        data_dir = %context.connection_info().data_dir,
        config_dirs = ?context.connection_info().config_dir_candidate_paths(),
        source = ?active.source,
        requested_name = active.requested_name.as_deref().unwrap_or(""),
        stack = ?active.stack,
        "active theme resolved",
    );
}

fn publish_runtime_appearance(theme: &ResolvedTheme) {
    let appearance = theme.appearance.clone();
    if bmux_plugin::global_event_bus()
        .publish_state(&RUNTIME_APPEARANCE_STATE_KIND, appearance.clone())
        .is_err()
    {
        let _ = bmux_plugin::global_event_bus()
            .register_state_channel_with_decoder::<RuntimeAppearance>(
                RUNTIME_APPEARANCE_STATE_KIND,
                appearance,
            );
    }
}

fn publish_runtime_appearance_to_host(context: &impl ServiceCaller, theme: &ResolvedTheme) {
    publish_runtime_appearance(theme);
    let appearance = theme.appearance.clone();
    let Ok(payload) = serde_json::to_vec(&appearance) else {
        return;
    };
    let response = context.execute_kernel_request(IpcRequest::EmitOnPluginBus {
        kind: RUNTIME_APPEARANCE_STATE_KIND.as_str().to_string(),
        payload,
    });
    if let Err(error) = response {
        warn!(%error, "failed relaying runtime appearance to host event bus");
    }
}

async fn run_theme_picker(context: NativeCommandContext) {
    let settings = parse_settings(context.settings.as_ref());
    let catalog = load_theme_catalog(&context.connection.config_dir_candidate_paths());
    if catalog.is_empty() {
        warn!("theme picker opened with empty catalog");
        return;
    }

    let active_stack = active_theme_stack(&context, &settings, &catalog);
    let active_name = active_stack
        .stack
        .iter()
        .find(|name| name.as_str() != "mode-aware")
        .cloned()
        .unwrap_or_else(|| "default".to_string());
    let Some(original_theme) =
        resolve_theme_stack_with_settings(&catalog, &active_stack.stack, &settings)
    else {
        return;
    };
    let all_plugin_ids = theme_catalog_plugin_ids(&catalog);
    publish_runtime_appearance_to_host(&context, &original_theme);
    apply_theme_extensions(
        &context,
        &original_theme,
        &all_plugin_ids,
        &context.connection.config_dir_candidates,
    );

    let request = bmux_plugin_sdk::PromptRequest::single_select(
        "Select Theme",
        prompt_options(&catalog, &active_stack.stack, &active_name),
    )
    .message("Move to preview live. Enter applies. Esc restores previous theme.")
    .single_default_index(selected_index(&catalog, &active_name))
    .single_live_preview(true)
    .policy(bmux_plugin_sdk::PromptPolicy::RejectIfBusy)
    .width_range(48, 96);

    let Ok((mut response_rx, mut event_rx)) = prompt::submit_with_events(request) else {
        warn!("theme picker prompt host unavailable");
        return;
    };

    let selected_name = loop {
        tokio::select! {
            response = &mut response_rx => {
                break match response {
                    Ok(PromptResponse::Submitted(PromptValue::Single(name))) => Some(name),
                    Ok(PromptResponse::Cancelled | PromptResponse::RejectedBusy | PromptResponse::Submitted(_)) | Err(_) => None,
                };
            }
            event = event_rx.recv() => {
                if let Some(PromptEvent::SelectionChanged { value, .. }) = event
                    && let Some(theme) = resolve_theme_stack_with_settings(&catalog, &base_theme_stack(&value), &settings)
                {
                    publish_runtime_appearance_to_host(&context, &theme);
                    apply_theme_extensions(
                        &context,
                        &theme,
                        &all_plugin_ids,
                        &context.connection.config_dir_candidates,
                    );
                }
            }
        }
    };

    if let Some(name) = selected_name
        && let Some(theme) =
            resolve_theme_stack_with_settings(&catalog, &base_theme_stack(&name), &settings)
    {
        publish_runtime_appearance_to_host(&context, &theme);
        apply_theme_extensions(
            &context,
            &theme,
            &all_plugin_ids,
            &context.connection.config_dir_candidates,
        );
        if matches!(
            settings.persistence,
            ThemePersistence::PersistBetweenConnects
        ) {
            persist_theme_name(&context, &name);
        } else {
            info!(
                theme = %name,
                persistence = ?settings.persistence,
                "theme selection not persisted because persistence is disabled",
            );
        }
        if name == "performance" {
            configure_performance_theme_header(&context, settings.persistence).await;
        }
        info!(theme = %name, persistence = ?settings.persistence, "theme selected");
        return;
    }

    publish_runtime_appearance_to_host(&context, &original_theme);
    apply_theme_extensions(
        &context,
        &original_theme,
        &all_plugin_ids,
        &context.connection.config_dir_candidates,
    );
}

async fn configure_performance_theme_header(
    context: &NativeCommandContext,
    persistence: ThemePersistence,
) {
    let response = match context
        .call_service::<(), bmux_performance_plugin_api::performance_types::PromptForm>(
            capabilities::PERFORMANCE_READ.as_str(),
            ServiceKind::Query,
            performance_state::INTERFACE_ID.as_str(),
            performance_state::OP_BUILD_THEME_HEADER_SETTINGS_FORM.as_str(),
            &(),
        ) {
        Ok(response) => response,
        Err(error) => {
            warn!(%error, "failed building performance theme settings form");
            return;
        }
    };
    let Ok(request) = bmux_plugin_sdk::PromptRequest::try_from(response) else {
        warn!("performance settings form service returned invalid prompt form");
        return;
    };
    let request = request
        .owner_plugin_id("bmux.theme")
        .modal_id("performance-advanced-settings")
        .policy(bmux_plugin_sdk::PromptPolicy::Enqueue);
    let response = match prompt::request(request).await {
        Ok(response) => response,
        Err(error) => {
            warn!(%error, "failed opening performance theme settings form");
            return;
        }
    };
    let PromptResponse::Submitted(PromptValue::Form(values)) = response else {
        return;
    };
    let values = values
        .into_iter()
        .map(|(key, value)| (key, value.into()))
        .collect();
    let request = performance_commands::client::ApplyThemeHeaderSettingsFormRequest { values };
    let response = context
        .call_service::<_, bmux_performance_plugin_api::performance_types::ThemeHeaderSettings>(
            capabilities::PERFORMANCE_WRITE.as_str(),
            ServiceKind::Command,
            performance_commands::INTERFACE_ID.as_str(),
            performance_commands::OP_APPLY_THEME_HEADER_SETTINGS_FORM.as_str(),
            &request,
        );
    let settings = match response {
        Ok(settings) => settings,
        Err(error) => {
            warn!(%error, "failed applying performance theme settings form");
            return;
        }
    };
    if matches!(persistence, ThemePersistence::PersistBetweenConnects) {
        persist_performance_theme_settings(context, &settings);
    } else {
        info!(
            persistence = ?persistence,
            "performance theme settings not persisted because persistence is disabled",
        );
    }
}

fn persist_performance_theme_settings(
    context: &impl ServiceCaller,
    settings: &bmux_performance_plugin_api::ThemeHeaderSettings,
) {
    let Ok(value) = serde_json::to_vec(settings) else {
        return;
    };
    let request = StorageSetRequest::new(
        bmux_plugin_sdk::storage_key!("theme_settings.performance"),
        value,
    );
    if let Err(error) = context.storage_set(&request) {
        warn!(%error, key = STORAGE_PERFORMANCE_THEME_SETTINGS, "failed persisting performance theme settings");
    } else {
        info!(
            key = STORAGE_PERFORMANCE_THEME_SETTINGS,
            "persisted performance theme settings"
        );
    }
}

fn apply_persisted_performance_theme_settings(context: &impl ServiceCaller) {
    let request =
        StorageGetRequest::new(bmux_plugin_sdk::storage_key!("theme_settings.performance"));
    let Ok(response) = context.storage_get(&request) else {
        warn!(
            key = STORAGE_PERFORMANCE_THEME_SETTINGS,
            "failed reading persisted performance theme settings"
        );
        return;
    };
    let Some(value) = response.value else {
        info!(
            key = STORAGE_PERFORMANCE_THEME_SETTINGS,
            "no persisted performance theme settings found"
        );
        return;
    };
    let Ok(settings) =
        serde_json::from_slice::<bmux_performance_plugin_api::ThemeHeaderSettings>(&value)
    else {
        warn!(
            key = STORAGE_PERFORMANCE_THEME_SETTINGS,
            "failed decoding persisted performance theme settings"
        );
        return;
    };
    info!(
        key = STORAGE_PERFORMANCE_THEME_SETTINGS,
        "applying persisted performance theme settings"
    );
    let request = performance_commands::client::SetThemeHeaderSettingsRequest { settings };
    if let Err(error) = context
        .call_service::<_, bmux_performance_plugin_api::performance_types::ThemeHeaderSettings>(
            capabilities::PERFORMANCE_WRITE.as_str(),
            ServiceKind::Command,
            performance_commands::INTERFACE_ID.as_str(),
            performance_commands::OP_SET_THEME_HEADER_SETTINGS.as_str(),
            &request,
        )
    {
        warn!(%error, "failed applying persisted performance theme settings");
    } else {
        info!(
            key = STORAGE_PERFORMANCE_THEME_SETTINGS,
            "applied persisted performance theme settings"
        );
    }
}

fn parse_settings(settings: Option<&toml::Value>) -> ThemePluginSettings {
    settings
        .cloned()
        .and_then(|value| value.try_into().ok())
        .unwrap_or_default()
}

fn declared_theme_name(settings: &ThemePluginSettings) -> String {
    settings
        .theme
        .as_deref()
        .map_or_else(|| "default".to_string(), normalized_theme_name)
}

fn declared_theme_stack(settings: &ThemePluginSettings) -> Vec<String> {
    if !settings.themes.is_empty() {
        return settings
            .themes
            .iter()
            .map(|name| normalized_theme_name(name))
            .collect();
    }
    base_theme_stack(&declared_theme_name(settings))
}

fn base_theme_stack(name: &str) -> Vec<String> {
    let base = normalized_theme_name(name);
    if base == "mode-aware" {
        vec![base]
    } else {
        vec![base, "mode-aware".to_string()]
    }
}

fn active_theme_stack(
    context: &(impl ThemeHostContext + ?Sized),
    settings: &ThemePluginSettings,
    catalog: &[ThemeCatalogEntry],
) -> ActiveThemeStack {
    if !settings.themes.is_empty() {
        let requested = declared_theme_stack(settings);
        return active_stack_from_requested(
            catalog,
            requested,
            ActiveThemeSource::DeclaredStack,
            settings.themes.first().cloned(),
        );
    }
    if matches!(
        settings.persistence,
        ThemePersistence::PersistBetweenConnects
    ) && let Some(name) = read_persisted_theme_name(context)
    {
        if theme_by_name(catalog, &name).is_some() {
            info!(theme = %name, "using persisted theme selection");
            let stack = filter_existing_theme_names(catalog, base_theme_stack(&name));
            return ActiveThemeStack {
                stack,
                source: ActiveThemeSource::Persisted,
                requested_name: Some(name),
            };
        }
        warn!(theme = %name, "persisted theme no longer exists in catalog; falling back to declared theme");
    }
    active_stack_from_requested(
        catalog,
        declared_theme_stack(settings),
        ActiveThemeSource::Declared,
        settings.theme.clone(),
    )
}

fn filter_existing_theme_names(catalog: &[ThemeCatalogEntry], names: Vec<String>) -> Vec<String> {
    names
        .into_iter()
        .filter(|name| theme_by_name(catalog, name).is_some())
        .collect()
}

fn active_stack_from_requested(
    catalog: &[ThemeCatalogEntry],
    requested: Vec<String>,
    source: ActiveThemeSource,
    requested_name: Option<String>,
) -> ActiveThemeStack {
    let primary_exists = requested
        .first()
        .is_some_and(|name| name == "mode-aware" || theme_by_name(catalog, name).is_some());
    let filtered = filter_existing_theme_names(catalog, requested);
    if !primary_exists || filtered.is_empty() {
        ActiveThemeStack {
            stack: base_theme_stack("default"),
            source: ActiveThemeSource::DefaultFallback,
            requested_name,
        }
    } else {
        ActiveThemeStack {
            stack: filtered,
            source,
            requested_name,
        }
    }
}

fn resolve_theme_stack(catalog: &[ThemeCatalogEntry], stack: &[String]) -> Option<ResolvedTheme> {
    if stack.is_empty() {
        return None;
    }
    let mut appearance = RuntimeAppearance::default();
    let mut plugins = BTreeMap::new();
    for name in stack {
        let theme = theme_by_name(catalog, name)?;
        apply_theme_layer(&mut appearance, &mut plugins, theme);
    }
    Some(ResolvedTheme {
        appearance,
        plugins,
    })
}

fn resolve_theme_stack_with_settings(
    catalog: &[ThemeCatalogEntry],
    stack: &[String],
    settings: &ThemePluginSettings,
) -> Option<ResolvedTheme> {
    let mut theme = resolve_theme_stack(catalog, stack)?;
    apply_settings_component_overrides(&mut theme, &settings.components);
    Some(theme)
}

fn apply_settings_component_overrides(
    theme: &mut ResolvedTheme,
    components: &BTreeMap<String, toml::Value>,
) {
    if components.is_empty() {
        return;
    }
    let components_table = components
        .iter()
        .map(|(id, value)| (id.clone(), value.clone()))
        .collect::<toml::map::Map<_, _>>();
    let overlay = toml::Value::Table(toml::map::Map::from_iter([(
        "components".to_string(),
        toml::Value::Table(components_table),
    )]));
    let extension = theme
        .plugins
        .entry("bmux.decoration".to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    merge_toml_value(extension, &overlay);
}

fn apply_theme_layer(
    appearance: &mut RuntimeAppearance,
    plugins: &mut BTreeMap<String, toml::Value>,
    theme: &ThemeConfig,
) {
    appearance.apply_patch(&RuntimeAppearancePatch::from(theme));
    for (mode_id, mode_theme) in &theme.modes {
        let patch = appearance
            .modes
            .entry(normalized_theme_name(mode_id))
            .or_default();
        patch.merge(&RuntimeAppearancePatch::from(mode_theme));
    }
    for (plugin_id, extension) in &theme.plugins {
        match plugins.get_mut(plugin_id) {
            Some(existing) => merge_toml_value(existing, extension),
            None => {
                plugins.insert(plugin_id.clone(), extension.clone());
            }
        }
    }
}

fn merge_toml_value(base: &mut toml::Value, overlay: &toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base_table), toml::Value::Table(overlay_table)) => {
            for (key, value) in overlay_table {
                match base_table.get_mut(key) {
                    Some(existing) => merge_toml_value(existing, value),
                    None => {
                        base_table.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base_value, overlay_value) => {
            *base_value = overlay_value.clone();
        }
    }
}

fn read_persisted_theme_name(context: &(impl ThemeHostContext + ?Sized)) -> Option<String> {
    let response = match context.storage_get(&StorageGetRequest::new(
        bmux_plugin_sdk::storage_key!("selected_theme"),
    )) {
        Ok(response) => response,
        Err(error) => {
            warn!(
                %error,
                data_dir = %context.connection_info().data_dir,
                key = STORAGE_SELECTED_APPEARANCE,
                "failed reading persisted theme selection",
            );
            return None;
        }
    };
    let Some(value) = response.value else {
        info!(
            data_dir = %context.connection_info().data_dir,
            key = STORAGE_SELECTED_APPEARANCE,
            "no persisted theme selection found",
        );
        return None;
    };
    match String::from_utf8(value) {
        Ok(name) => {
            let normalized = normalized_theme_name(&name);
            info!(
                data_dir = %context.connection_info().data_dir,
                key = STORAGE_SELECTED_APPEARANCE,
                theme = %normalized,
                "persisted theme selection read",
            );
            Some(normalized)
        }
        Err(error) => {
            warn!(
                %error,
                data_dir = %context.connection_info().data_dir,
                key = STORAGE_SELECTED_APPEARANCE,
                "persisted theme selection was not valid UTF-8",
            );
            None
        }
    }
}

fn persist_theme_name(context: &impl ThemeHostContext, name: &str) {
    let result = context.storage_set(&StorageSetRequest::new(
        bmux_plugin_sdk::storage_key!("selected_theme"),
        name.as_bytes().to_vec(),
    ));
    if let Err(error) = result {
        warn!(
            %error,
            data_dir = %context.connection_info().data_dir,
            key = STORAGE_SELECTED_APPEARANCE,
            theme = %name,
            "failed persisting selected theme",
        );
    } else {
        info!(
            data_dir = %context.connection_info().data_dir,
            key = STORAGE_SELECTED_APPEARANCE,
            theme = %name,
            "persisted selected theme",
        );
    }
}

fn load_theme_catalog(config_dir_candidates: &[std::path::PathBuf]) -> Vec<ThemeCatalogEntry> {
    let mut entries = vec![ThemeCatalogEntry {
        name: "default".to_string(),
        theme: ThemeConfig::default(),
    }];

    for dir in config_dir_candidates {
        let themes_dir = dir.join("themes");
        let Ok(read_dir) = std::fs::read_dir(themes_dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(std::ffi::OsStr::to_str) == Some("toml") {
                load_theme_file(&path, &mut entries);
            }
        }
    }

    for (name, text) in bundled_theme_presets() {
        if let Ok(theme) = toml::from_str::<ThemeConfig>(text) {
            upsert_theme_catalog_entry(&mut entries, (*name).to_string(), theme);
        }
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

fn load_theme_file(path: &Path, entries: &mut Vec<ThemeCatalogEntry>) {
    let Some(name) = path.file_stem().and_then(std::ffi::OsStr::to_str) else {
        return;
    };
    if let Ok(text) = std::fs::read_to_string(path)
        && let Ok(theme) = toml::from_str::<ThemeConfig>(&text)
    {
        upsert_theme_catalog_entry(entries, name.to_string(), theme);
    }
}

const fn bundled_theme_presets() -> &'static [(&'static str, &'static str)] {
    &[
        ("hacker", include_str!("../assets/themes/hacker.toml")),
        ("cyberpunk", include_str!("../assets/themes/cyberpunk.toml")),
        ("minimal", include_str!("../assets/themes/minimal.toml")),
        (
            "pulse-demo",
            include_str!("../assets/themes/pulse-demo.toml"),
        ),
        (
            "rainbow-snake",
            include_str!("../assets/themes/rainbow-snake.toml"),
        ),
        (
            "performance",
            include_str!("../assets/themes/performance.toml"),
        ),
        ("pong", include_str!("../assets/themes/pong.toml")),
        (
            "mode-aware",
            include_str!("../assets/themes/mode-aware.toml"),
        ),
    ]
}

fn upsert_theme_catalog_entry(
    entries: &mut Vec<ThemeCatalogEntry>,
    name: String,
    mut theme: ThemeConfig,
) {
    theme.name.clone_from(&name);
    if let Some(existing) = entries.iter_mut().find(|entry| entry.name == name) {
        existing.theme = theme;
    } else {
        entries.push(ThemeCatalogEntry { name, theme });
    }
}

fn theme_catalog_plugin_ids(catalog: &[ThemeCatalogEntry]) -> Vec<String> {
    catalog
        .iter()
        .flat_map(|entry| entry.theme.plugins.keys().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn prompt_options(
    catalog: &[ThemeCatalogEntry],
    declared_stack: &[String],
    active_name: &str,
) -> Vec<bmux_plugin_sdk::PromptOption> {
    catalog
        .iter()
        .map(|entry| {
            let mut label = entry.name.clone();
            if declared_stack.iter().any(|name| name == &entry.name) {
                label.push_str(" (declared)");
            }
            if entry.name == active_name {
                label.push_str(" (active)");
            }
            bmux_plugin_sdk::PromptOption::new(entry.name.as_str(), label)
        })
        .collect()
}

fn selected_index(catalog: &[ThemeCatalogEntry], active_name: &str) -> usize {
    catalog
        .iter()
        .position(|entry| entry.name == active_name)
        .unwrap_or(0)
}

fn theme_by_name<'a>(catalog: &'a [ThemeCatalogEntry], name: &str) -> Option<&'a ThemeConfig> {
    catalog
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| &entry.theme)
}

fn apply_theme_extensions(
    context: &impl ServiceCaller,
    theme: &ResolvedTheme,
    plugin_ids: &[String],
    config_dir_candidates: &[String],
) {
    for plugin_id in plugin_ids {
        let toml = theme
            .plugins
            .get(plugin_id)
            .and_then(|extension| toml::to_string(extension).ok())
            .unwrap_or_default();
        let request = ApplyThemeExtensionArgs {
            toml,
            config_dir_candidates: config_dir_candidates.to_vec(),
        };
        let has_extension = !request.toml.trim().is_empty();
        let Ok(payload) = bmux_plugin_sdk::encode_service_message(&request) else {
            warn!(plugin_id = %plugin_id, "failed encoding theme extension apply request");
            continue;
        };
        let capability = format!("{plugin_id}.write");
        info!(
            plugin_id = %plugin_id,
            capability = %capability,
            has_extension,
            "applying theme extension",
        );
        if let Err(error) = execute_theme_extension_apply(context, &capability, payload) {
            warn!(
                %error,
                plugin_id = %plugin_id,
                capability = %capability,
                interface = "theme-extension",
                operation = "apply",
                "theme extension apply failed",
            );
        } else {
            info!(
                plugin_id = %plugin_id,
                capability = %capability,
                has_extension,
                "theme extension apply completed",
            );
        }
    }
}

fn execute_theme_extension_apply(
    context: &impl ServiceCaller,
    capability: &str,
    payload: Vec<u8>,
) -> std::result::Result<(), String> {
    context
        .call_service_raw(
            capability,
            ServiceKind::Command,
            "theme-extension",
            "apply",
            payload,
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn normalized_theme_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin::test_support::{TestServiceRouter, install_test_service_router};
    use bmux_plugin_sdk::{
        ApiVersion, HostMetadata, HostScope, PluginEventKind, ProviderId, RegisteredService,
        ServiceKind, ServiceRequest, StorageGetResponse, decode_service_message,
        encode_service_message,
    };
    use std::sync::{Arc, Mutex};

    #[test]
    fn active_appearance_service_uses_declared_theme() {
        let mut plugin = ThemePlugin::default();
        let context = service_context(Some(toml::Value::Table(toml::map::Map::from_iter([(
            "theme".to_string(),
            toml::Value::String("rainbow-snake".to_string()),
        )]))));

        let response = plugin.invoke_service(context);

        assert!(response.error.is_none(), "unexpected error: {response:?}");
        let appearance: RuntimeAppearance =
            decode_service_message(&response.payload).expect("appearance response should decode");
        assert_eq!(appearance.background, "#050510");
        assert_eq!(appearance.border.active, "#ffffff");
        assert!(appearance.modes.contains_key("normal"));
    }

    #[test]
    fn active_appearance_service_uses_persisted_theme_when_enabled() {
        let _router = install_persisted_theme_router(Some("hacker"));
        let mut plugin = ThemePlugin::default();
        let context = service_context(Some(toml::Value::Table(toml::map::Map::from_iter([
            (
                "theme".to_string(),
                toml::Value::String("pulse-demo".to_string()),
            ),
            (
                "persistence".to_string(),
                toml::Value::String("persist_between_connects".to_string()),
            ),
        ]))));

        let response = plugin.invoke_service(context);

        assert!(response.error.is_none(), "unexpected error: {response:?}");
        let appearance: RuntimeAppearance =
            decode_service_message(&response.payload).expect("appearance response should decode");
        assert_eq!(appearance.foreground, "#39ff14");
        assert_eq!(appearance.border.active, "#39ff14");
    }

    #[test]
    fn configured_theme_uses_persisted_plugin_extensions_when_enabled() {
        let _router = install_persisted_theme_router(Some("hacker"));
        let context = service_context(Some(toml::Value::Table(toml::map::Map::from_iter([
            (
                "theme".to_string(),
                toml::Value::String("pulse-demo".to_string()),
            ),
            (
                "persistence".to_string(),
                toml::Value::String("persist_between_connects".to_string()),
            ),
        ]))));

        let active = configured_theme(&context).expect("active theme should resolve");

        assert_eq!(active.source, ActiveThemeSource::Persisted);
        assert_eq!(active.requested_name.as_deref(), Some("hacker"));
        let decoration = active
            .theme
            .plugins
            .get("bmux.decoration")
            .and_then(toml::Value::as_table)
            .expect("hacker decoration extension should resolve");
        assert!(decoration.get("script").is_none());
        assert_eq!(
            decoration
                .get("focused")
                .and_then(toml::Value::as_table)
                .and_then(|table| table.get("style"))
                .and_then(toml::Value::as_str),
            Some("thick"),
        );
    }

    #[test]
    fn explicit_theme_stack_ignores_persisted_theme() {
        let _router = install_persisted_theme_router(Some("hacker"));
        let context = service_context(Some(toml::Value::Table(toml::map::Map::from_iter([
            (
                "themes".to_string(),
                toml::Value::Array(vec![toml::Value::String("rainbow-snake".to_string())]),
            ),
            (
                "persistence".to_string(),
                toml::Value::String("persist_between_connects".to_string()),
            ),
        ]))));

        let active = configured_theme(&context).expect("active theme should resolve");

        assert_eq!(active.source, ActiveThemeSource::DeclaredStack);
        assert_eq!(active.theme.appearance.background, "#050510");
    }

    #[test]
    fn unknown_persisted_theme_falls_back_to_declared_theme() {
        let _router = install_persisted_theme_router(Some("missing-theme"));
        let context = service_context(Some(toml::Value::Table(toml::map::Map::from_iter([
            (
                "theme".to_string(),
                toml::Value::String("pulse-demo".to_string()),
            ),
            (
                "persistence".to_string(),
                toml::Value::String("persist_between_connects".to_string()),
            ),
        ]))));

        let active = configured_theme(&context).expect("active theme should resolve");

        assert_eq!(active.source, ActiveThemeSource::Declared);
        assert_eq!(active.theme.appearance.foreground, "#e6ffe6");
    }

    #[test]
    fn startup_theme_extensions_use_persisted_theme_via_service_routing() {
        let applied = Arc::new(Mutex::new(Vec::new()));
        let _router =
            install_persisted_theme_extension_router(Some("hacker"), Arc::clone(&applied));
        let context = lifecycle_context(Some(toml::Value::Table(toml::map::Map::from_iter([
            (
                "theme".to_string(),
                toml::Value::String("pulse-demo".to_string()),
            ),
            (
                "persistence".to_string(),
                toml::Value::String("persist_between_connects".to_string()),
            ),
        ]))));

        apply_configured_theme_extensions(&context);

        let extension_toml = {
            let applied = applied.lock().expect("applied extensions lock should hold");
            assert_eq!(applied.len(), 1);
            applied[0].toml.clone()
        };
        let extension =
            toml::from_str::<toml::Value>(&extension_toml).expect("extension toml should parse");
        let table = extension
            .as_table()
            .expect("extension should be a toml table");
        assert!(table.get("script").is_none());
        assert_eq!(
            table
                .get("focused")
                .and_then(toml::Value::as_table)
                .and_then(|focused| focused.get("style"))
                .and_then(toml::Value::as_str),
            Some("thick"),
        );
    }

    #[test]
    fn activate_applies_persisted_theme_extensions_immediately() {
        let applied = Arc::new(Mutex::new(Vec::new()));
        let _router =
            install_persisted_theme_extension_router(Some("hacker"), Arc::clone(&applied));
        let context = lifecycle_context(Some(toml::Value::Table(toml::map::Map::from_iter([
            (
                "theme".to_string(),
                toml::Value::String("pulse-demo".to_string()),
            ),
            (
                "persistence".to_string(),
                toml::Value::String("persist_between_connects".to_string()),
            ),
        ]))));
        let mut plugin = ThemePlugin::default();

        plugin
            .activate(context)
            .expect("theme activation should apply extensions");

        let applied_toml = {
            let applied = applied.lock().expect("applied extensions lock should hold");
            assert_eq!(applied.len(), 1);
            applied[0].toml.clone()
        };
        assert!(
            applied_toml.contains("style = \"thick\""),
            "activation should apply persisted hacker decoration extension: {applied_toml}",
        );
    }

    #[test]
    fn performance_theme_settings_use_storage_safe_key_for_persist_and_restore() {
        let settings = bmux_performance_plugin_api::ThemeHeaderSettings {
            sample_interval_ms: 2_500,
            ..bmux_performance_plugin_api::ThemeHeaderSettings::default()
        };
        let settings_bytes = serde_json::to_vec(&settings).expect("settings should encode");
        let stored_keys = Arc::new(Mutex::new(Vec::new()));
        let applied_settings = Arc::new(Mutex::new(Vec::new()));
        let _router = install_performance_theme_settings_router(
            settings_bytes,
            Arc::clone(&stored_keys),
            Arc::clone(&applied_settings),
        );
        let context = lifecycle_context(None);

        persist_performance_theme_settings(&context, &settings);
        apply_persisted_performance_theme_settings(&context);

        let stored_keys_snapshot = {
            let stored_keys = stored_keys.lock().expect("stored key lock should hold");
            stored_keys.clone()
        };
        assert_eq!(
            stored_keys_snapshot.as_slice(),
            [STORAGE_PERFORMANCE_THEME_SETTINGS]
        );
        let applied_settings_snapshot = {
            let applied_settings = applied_settings
                .lock()
                .expect("applied settings lock should hold");
            applied_settings.clone()
        };
        assert_eq!(applied_settings_snapshot.as_slice(), [settings]);
    }

    #[test]
    fn server_started_republishes_persisted_runtime_appearance() {
        let applied = Arc::new(Mutex::new(Vec::new()));
        let _router =
            install_persisted_theme_extension_router(Some("hacker"), Arc::clone(&applied));
        bmux_plugin::global_event_bus().register_state_channel::<RuntimeAppearance>(
            RUNTIME_APPEARANCE_STATE_KIND,
            RuntimeAppearance::default(),
        );
        let context = lifecycle_context(Some(toml::Value::Table(toml::map::Map::from_iter([
            (
                "theme".to_string(),
                toml::Value::String("pulse-demo".to_string()),
            ),
            (
                "persistence".to_string(),
                toml::Value::String("persist_between_connects".to_string()),
            ),
        ]))));
        let mut plugin = ThemePlugin {
            lifecycle_context: Some(context),
        };

        plugin
            .handle_event(PluginEvent {
                kind: PluginEventKind::from_owned("bmux.core/server_started".to_string()),
                payload: serde_json::json!({}),
            })
            .expect("server_started should reapply theme");

        let (appearance, _rx) = bmux_plugin::global_event_bus()
            .subscribe_state::<RuntimeAppearance>(&RUNTIME_APPEARANCE_STATE_KIND)
            .expect("runtime appearance state should be registered");
        assert_eq!(appearance.foreground, "#39ff14");
        assert_eq!(appearance.border.active, "#39ff14");
        let applied_toml = {
            let applied = applied.lock().expect("applied extensions lock should hold");
            assert_eq!(applied.len(), 1);
            applied[0].toml.clone()
        };
        assert!(
            applied_toml.contains("style = \"thick\""),
            "server_started should also apply persisted decoration extension: {applied_toml}",
        );
    }

    #[test]
    fn theme_stack_layers_are_additive() {
        let base: ThemeConfig = toml::from_str(
            r##"
            foreground = "#111111"
            background = "#222222"

            [status]
            foreground = "#333333"
            mode_indicator = "#444444"
            "##,
        )
        .expect("base theme parses");
        let overlay: ThemeConfig = toml::from_str(
            r##"
            cursor = "#555555"

            [status]
            mode_indicator = "#666666"

            [modes.normal.status]
            mode_indicator = "#777777"

            [modes.normal.content_effects.default_bg_wash]
            enabled = true
            scope = "cells"
            when_bg = "default"
            background_blend = { color = "#ff0000", amount = 0.16 }
            "##,
        )
        .expect("overlay theme parses");
        let catalog = vec![
            ThemeCatalogEntry {
                name: "base".to_string(),
                theme: base,
            },
            ThemeCatalogEntry {
                name: "overlay".to_string(),
                theme: overlay,
            },
        ];

        let resolved = resolve_theme_stack(&catalog, &["base".to_string(), "overlay".to_string()])
            .expect("stack resolves");

        assert_eq!(resolved.appearance.foreground, "#111111");
        assert_eq!(resolved.appearance.background, "#222222");
        assert_eq!(resolved.appearance.cursor, "#555555");
        assert_eq!(resolved.appearance.status.foreground, "#333333");
        assert_eq!(resolved.appearance.status.mode_indicator, "#666666");
        assert_eq!(
            resolved.appearance.for_mode("normal").status.mode_indicator,
            "#777777"
        );
        let normal = resolved.appearance.for_mode("normal");
        let effect = normal
            .content_effects
            .get("default_bg_wash")
            .expect("normal mode wash effect should resolve");
        assert!(effect.enabled);
        let blend = effect
            .background_blend
            .as_ref()
            .expect("background blend should resolve");
        assert_eq!(blend.color, "#ff0000");
        assert_eq!(blend.amount_permille, 160);
    }

    #[test]
    fn content_effect_layers_merge_by_name() {
        let lower: ThemeConfig = toml::from_str(
            r##"
            [content_effects.default_bg_wash]
            enabled = true
            scope = "cells"
            when_bg = "default"
            background_blend = { color = "#ff0000", amount = 0.16 }
            "##,
        )
        .expect("lower theme parses");
        let upper: ThemeConfig = toml::from_str(
            r"
            [content_effects.default_bg_wash]
            background_blend = { amount = 0.08 }
            ",
        )
        .expect("upper theme parses");
        let catalog = vec![
            ThemeCatalogEntry {
                name: "lower".to_string(),
                theme: lower,
            },
            ThemeCatalogEntry {
                name: "upper".to_string(),
                theme: upper,
            },
        ];

        let resolved = resolve_theme_stack(&catalog, &["lower".to_string(), "upper".to_string()])
            .expect("stack resolves");
        let effect = resolved
            .appearance
            .content_effects
            .get("default_bg_wash")
            .expect("effect should resolve");
        let blend = effect
            .background_blend
            .as_ref()
            .expect("background blend should resolve");

        assert_eq!(blend.color, "#ff0000");
        assert_eq!(blend.amount_permille, 80);
    }

    #[test]
    fn plugin_extensions_merge_deeply() {
        let lower: ThemeConfig = toml::from_str(
            r##"
            [plugins."bmux.decoration".focused]
            fg = "#111111"
            style = "rounded"
            "##,
        )
        .expect("lower theme parses");
        let upper: ThemeConfig = toml::from_str(
            r##"
            [plugins."bmux.decoration".focused]
            fg = "#222222"
            "##,
        )
        .expect("upper theme parses");
        let catalog = vec![
            ThemeCatalogEntry {
                name: "lower".to_string(),
                theme: lower,
            },
            ThemeCatalogEntry {
                name: "upper".to_string(),
                theme: upper,
            },
        ];

        let resolved = resolve_theme_stack(&catalog, &["lower".to_string(), "upper".to_string()])
            .expect("stack resolves");
        let extension = resolved
            .plugins
            .get("bmux.decoration")
            .and_then(|value| value.as_table())
            .and_then(|table| table.get("focused"))
            .and_then(|value| value.as_table())
            .expect("focused extension exists");

        assert_eq!(
            extension.get("fg").and_then(toml::Value::as_str),
            Some("#222222")
        );
        assert_eq!(
            extension.get("style").and_then(toml::Value::as_str),
            Some("rounded")
        );
    }

    #[test]
    fn settings_components_apply_final_decoration_component_overrides() {
        let base: ThemeConfig = toml::from_str(
            r#"
            [plugins."bmux.decoration".components."performance.border"]
            script = "performance_header"

            [plugins."bmux.decoration".components.snake]
            script = "rainbow_snake"
            "#,
        )
        .expect("base theme parses");
        let catalog = vec![ThemeCatalogEntry {
            name: "base".to_string(),
            theme: base,
        }];
        let settings = ThemePluginSettings {
            components: BTreeMap::from([(
                "snake".to_string(),
                toml::Value::Table(toml::map::Map::from_iter([
                    (
                        "above".to_string(),
                        toml::Value::Array(vec![toml::Value::String(
                            "performance.border".to_string(),
                        )]),
                    ),
                    (
                        "below".to_string(),
                        toml::Value::Array(vec![toml::Value::String(
                            "performance.header".to_string(),
                        )]),
                    ),
                ])),
            )]),
            ..ThemePluginSettings::default()
        };

        let resolved =
            resolve_theme_stack_with_settings(&catalog, &["base".to_string()], &settings)
                .expect("stack resolves");
        let snake = resolved
            .plugins
            .get("bmux.decoration")
            .and_then(toml::Value::as_table)
            .and_then(|table| table.get("components"))
            .and_then(toml::Value::as_table)
            .and_then(|components| components.get("snake"))
            .and_then(toml::Value::as_table)
            .expect("snake component exists");

        assert_eq!(
            snake.get("script").and_then(toml::Value::as_str),
            Some("rainbow_snake")
        );
        assert_eq!(
            snake
                .get("above")
                .and_then(toml::Value::as_array)
                .and_then(|values| values.first())
                .and_then(toml::Value::as_str),
            Some("performance.border")
        );
        assert_eq!(
            snake
                .get("below")
                .and_then(toml::Value::as_array)
                .and_then(|values| values.first())
                .and_then(toml::Value::as_str),
            Some("performance.header")
        );
    }

    #[test]
    fn decoration_component_extensions_merge_by_component_id() {
        let lower: ThemeConfig = toml::from_str(
            r#"
            [plugins."bmux.decoration".components."performance.header"]
            script = "performance_header"
            above = ["performance.border"]
            "#,
        )
        .expect("lower theme parses");
        let upper: ThemeConfig = toml::from_str(
            r#"
            [plugins."bmux.decoration".components."performance.header"]
            enabled = false
            below = ["snake.body"]
            "#,
        )
        .expect("upper theme parses");
        let catalog = vec![
            ThemeCatalogEntry {
                name: "lower".to_string(),
                theme: lower,
            },
            ThemeCatalogEntry {
                name: "upper".to_string(),
                theme: upper,
            },
        ];

        let resolved = resolve_theme_stack(&catalog, &["lower".to_string(), "upper".to_string()])
            .expect("stack resolves");
        let component = resolved
            .plugins
            .get("bmux.decoration")
            .and_then(toml::Value::as_table)
            .and_then(|table| table.get("components"))
            .and_then(toml::Value::as_table)
            .and_then(|components| components.get("performance.header"))
            .and_then(toml::Value::as_table)
            .expect("component extension exists");

        assert_eq!(
            component.get("script").and_then(toml::Value::as_str),
            Some("performance_header")
        );
        assert_eq!(
            component.get("enabled").and_then(toml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            component
                .get("above")
                .and_then(toml::Value::as_array)
                .and_then(|values| values.first())
                .and_then(toml::Value::as_str),
            Some("performance.border")
        );
        assert_eq!(
            component
                .get("below")
                .and_then(toml::Value::as_array)
                .and_then(|values| values.first())
                .and_then(toml::Value::as_str),
            Some("snake.body")
        );
    }

    fn service_context(settings: Option<toml::Value>) -> NativeServiceContext {
        NativeServiceContext {
            plugin_id: "bmux.theme".to_string(),
            request: ServiceRequest {
                caller_plugin_id: "test".to_string(),
                service: RegisteredService {
                    capability: HostScope::new("bmux.theme.read").expect("capability should parse"),
                    kind: ServiceKind::Query,
                    interface_id: "theme-state".to_string(),
                    provider: ProviderId::Plugin("bmux.theme".to_string()),
                },
                operation: "active-appearance".to_string(),
                payload: encode_service_message(&()).expect("unit payload should encode"),
            },
            required_capabilities: vec!["bmux.storage".to_string()],
            provided_capabilities: vec!["bmux.theme.read".to_string()],
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: vec!["bmux.theme".to_string()],
            plugin_search_roots: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.0.0-test".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: HostConnectionInfo {
                config_dir: String::new(),
                config_dir_candidates: Vec::new(),
                runtime_dir: String::new(),
                data_dir: String::new(),
                state_dir: String::new(),
            },
            settings,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            host_kernel_bridge: None,
        }
    }

    fn lifecycle_context(settings: Option<toml::Value>) -> NativeLifecycleContext {
        NativeLifecycleContext {
            plugin_id: "bmux.theme".to_string(),
            required_capabilities: vec![
                "bmux.storage".to_string(),
                "bmux.decoration.write".to_string(),
            ],
            provided_capabilities: vec![
                "bmux.theme.read".to_string(),
                "bmux.theme.write".to_string(),
            ],
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: vec!["bmux.theme".to_string(), "bmux.decoration".to_string()],
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.0.0-test".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: HostConnectionInfo {
                config_dir: String::new(),
                config_dir_candidates: Vec::new(),
                runtime_dir: String::new(),
                data_dir: String::new(),
                state_dir: String::new(),
            },
            settings,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: None,
        }
    }

    #[allow(clippy::result_large_err)] // Test router signature is fixed by bmux_plugin test support.
    fn install_persisted_theme_router(
        selected: Option<&'static str>,
    ) -> bmux_plugin::test_support::TestServiceRouterGuard {
        let router: TestServiceRouter = Arc::new(
            move |_caller_plugin_id,
                  _caller_client_id,
                  capability,
                  kind,
                  interface,
                  operation,
                  _payload| {
                assert_eq!(capability, "bmux.storage");
                assert_eq!(kind, ServiceKind::Query);
                assert_eq!(interface, "storage-query/v1");
                assert_eq!(operation, "get");
                encode_service_message(&StorageGetResponse {
                    value: selected.map(|value| value.as_bytes().to_vec()),
                })
            },
        );
        install_test_service_router(router)
    }

    #[allow(clippy::result_large_err)] // Test router signature is fixed by bmux_plugin test support.
    fn install_performance_theme_settings_router(
        settings_bytes: Vec<u8>,
        stored_keys: Arc<Mutex<Vec<String>>>,
        applied_settings: Arc<Mutex<Vec<bmux_performance_plugin_api::ThemeHeaderSettings>>>,
    ) -> bmux_plugin::test_support::TestServiceRouterGuard {
        let router: TestServiceRouter = Arc::new(
            move |_caller_plugin_id,
                  _caller_client_id,
                  capability,
                  kind,
                  interface,
                  operation,
                  payload| {
                match (capability, kind, interface, operation) {
                    ("bmux.storage", ServiceKind::Command, "storage-command/v1", "set") => {
                        let request: StorageSetRequest = decode_service_message(&payload)
                            .expect("storage set payload should decode");
                        stored_keys
                            .lock()
                            .expect("stored key lock should hold")
                            .push(request.key.into_string());
                        let stored_settings: bmux_performance_plugin_api::ThemeHeaderSettings =
                            serde_json::from_slice(&request.value)
                                .expect("stored performance settings should decode");
                        assert_eq!(stored_settings.sample_interval_ms, 2_500);
                        encode_service_message(&())
                    }
                    ("bmux.storage", ServiceKind::Query, "storage-query/v1", "get") => {
                        let request: StorageGetRequest = decode_service_message(&payload)
                            .expect("storage get payload should decode");
                        assert_eq!(request.key.as_str(), STORAGE_PERFORMANCE_THEME_SETTINGS);
                        encode_service_message(&StorageGetResponse {
                            value: Some(settings_bytes.clone()),
                        })
                    }
                    ("bmux.performance.write", ServiceKind::Command, interface, operation)
                        if interface == performance_commands::INTERFACE_ID.as_str()
                            && operation
                                == performance_commands::OP_SET_THEME_HEADER_SETTINGS.as_str() =>
                    {
                        let request: performance_commands::client::SetThemeHeaderSettingsRequest =
                            decode_service_message(&payload)
                                .expect("performance settings payload should decode");
                        applied_settings
                            .lock()
                            .expect("applied settings lock should hold")
                            .push(request.settings.clone());
                        encode_service_message(&request.settings)
                    }
                    other => panic!("unexpected service call: {other:?}"),
                }
            },
        );
        install_test_service_router(router)
    }

    #[allow(clippy::result_large_err)] // Test router signature is fixed by bmux_plugin test support.
    fn install_persisted_theme_extension_router(
        selected: Option<&'static str>,
        applied: Arc<Mutex<Vec<ApplyThemeExtensionArgs>>>,
    ) -> bmux_plugin::test_support::TestServiceRouterGuard {
        let router: TestServiceRouter = Arc::new(
            move |_caller_plugin_id,
                  _caller_client_id,
                  capability,
                  kind,
                  interface,
                  operation,
                  payload| {
                match (capability, kind, interface, operation) {
                    ("bmux.storage", ServiceKind::Query, "storage-query/v1", "get") => {
                        encode_service_message(&StorageGetResponse {
                            value: selected.map(|value| value.as_bytes().to_vec()),
                        })
                    }
                    ("bmux.decoration.write", ServiceKind::Command, "theme-extension", "apply") => {
                        let request: ApplyThemeExtensionArgs = decode_service_message(&payload)
                            .expect("theme extension payload should decode");
                        applied
                            .lock()
                            .expect("applied extensions lock should hold")
                            .push(request);
                        encode_service_message(&())
                    }
                    other => panic!("unexpected service call: {other:?}"),
                }
            },
        );
        install_test_service_router(router)
    }
}

bmux_plugin_sdk::export_plugin!(ThemePlugin, include_str!("../plugin.toml"));
