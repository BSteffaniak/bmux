#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![cfg_attr(feature = "static-bundled", allow(dead_code))]

use bmux_appearance::{
    RUNTIME_APPEARANCE_STATE_KIND, RuntimeAppearance, RuntimeBorderAppearance,
    RuntimeStatusAppearance,
};
use bmux_plugin::prompt;
use bmux_plugin::{HostRuntimeApi, ServiceCaller};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    PromptEvent, PromptResponse, PromptValue, ServiceKind, StorageGetRequest, StorageSetRequest,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tracing::{debug, warn};

const STORAGE_SELECTED_APPEARANCE: &str = "selected_theme";

#[derive(Default)]
pub struct ThemePlugin;

impl RustPlugin for ThemePlugin {
    fn activate(&mut self, context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        apply_configured_appearance(&context);
        Ok(EXIT_OK)
    }

    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        bmux_plugin_sdk::route_command!(context, {
            "pick-theme" => pick_theme(&context),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ThemeConfig {
    name: String,
    foreground: String,
    background: String,
    cursor: String,
    selection_background: String,
    border: BorderColors,
    status: StatusColors,
    #[serde(rename = "plugins", skip_serializing_if = "BTreeMap::is_empty")]
    plugins: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct BorderColors {
    active: String,
    inactive: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct StatusColors {
    background: String,
    foreground: String,
    active_window: String,
    mode_indicator: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            foreground: "#ffffff".to_string(),
            background: "#000000".to_string(),
            cursor: "#ffffff".to_string(),
            selection_background: "#404040".to_string(),
            border: BorderColors::default(),
            status: StatusColors::default(),
            plugins: BTreeMap::new(),
        }
    }
}

impl Default for BorderColors {
    fn default() -> Self {
        Self {
            active: "#00ff00".to_string(),
            inactive: "#808080".to_string(),
        }
    }
}

impl Default for StatusColors {
    fn default() -> Self {
        Self {
            background: "#1e1e1e".to_string(),
            foreground: "#ffffff".to_string(),
            active_window: "#00ff00".to_string(),
            mode_indicator: "#ffff00".to_string(),
        }
    }
}

impl From<&ThemeConfig> for RuntimeAppearance {
    fn from(theme: &ThemeConfig) -> Self {
        Self {
            foreground: theme.foreground.clone(),
            background: theme.background.clone(),
            cursor: theme.cursor.clone(),
            selection_background: theme.selection_background.clone(),
            border: RuntimeBorderAppearance {
                active: theme.border.active.clone(),
                inactive: theme.border.inactive.clone(),
            },
            status: RuntimeStatusAppearance {
                background: theme.status.background.clone(),
                foreground: theme.status.foreground.clone(),
                active_window: theme.status.active_window.clone(),
                mode_indicator: theme.status.mode_indicator.clone(),
            },
        }
    }
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
    persistence: ThemePersistence,
}

#[derive(Debug, Clone)]
struct ThemeCatalogEntry {
    name: String,
    theme: ThemeConfig,
}

#[derive(Debug, Serialize)]
struct ApplyThemeExtensionArgs {
    toml: String,
    config_dir_candidates: Vec<String>,
}

fn pick_theme(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let handle = tokio::runtime::Handle::try_current().map_err(|_| {
        PluginCommandError::unavailable("no tokio runtime available; theme picker requires attach")
    })?;
    handle.spawn(run_theme_picker(context.clone()));
    Ok(EXIT_OK)
}

fn apply_configured_appearance(context: &NativeLifecycleContext) {
    let settings = parse_settings(context.settings.as_ref());
    let catalog = load_theme_catalog(&context.connection.config_dir_candidate_paths());
    let declared_name = declared_theme_name(&settings);
    let active_name = active_theme_name(context, settings.persistence, &catalog, &declared_name);
    let Some(theme) = theme_by_name(&catalog, &active_name) else {
        return;
    };
    let all_plugin_ids = theme_catalog_plugin_ids(&catalog);
    publish_runtime_appearance(theme);
    apply_theme_extensions(
        context,
        theme,
        &all_plugin_ids,
        &context.connection.config_dir_candidates,
    );
}

fn publish_runtime_appearance(theme: &ThemeConfig) {
    let appearance = RuntimeAppearance::from(theme);
    if bmux_plugin::global_event_bus()
        .publish_state(&RUNTIME_APPEARANCE_STATE_KIND, appearance.clone())
        .is_err()
    {
        let _ = bmux_plugin::global_event_bus()
            .register_state_channel::<RuntimeAppearance>(RUNTIME_APPEARANCE_STATE_KIND, appearance);
    }
}

async fn run_theme_picker(context: NativeCommandContext) {
    let settings = parse_settings(context.settings.as_ref());
    let catalog = load_theme_catalog(&context.connection.config_dir_candidate_paths());
    if catalog.is_empty() {
        warn!("theme picker opened with empty catalog");
        return;
    }

    let declared_name = declared_theme_name(&settings);
    let active_name = active_theme_name(&context, settings.persistence, &catalog, &declared_name);
    let Some(original_theme) = theme_by_name(&catalog, &active_name).cloned() else {
        return;
    };
    let all_plugin_ids = theme_catalog_plugin_ids(&catalog);
    publish_runtime_appearance(&original_theme);
    apply_theme_extensions(
        &context,
        &original_theme,
        &all_plugin_ids,
        &context.connection.config_dir_candidates,
    );

    let request = bmux_plugin_sdk::PromptRequest::single_select(
        "Select Theme",
        prompt_options(&catalog, &declared_name, &active_name),
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
                    && let Some(theme) = theme_by_name(&catalog, &value)
                {
                    publish_runtime_appearance(theme);
                    apply_theme_extensions(
                        &context,
                        theme,
                        &all_plugin_ids,
                        &context.connection.config_dir_candidates,
                    );
                }
            }
        }
    };

    if let Some(name) = selected_name
        && let Some(theme) = theme_by_name(&catalog, &name)
    {
        publish_runtime_appearance(theme);
        apply_theme_extensions(
            &context,
            theme,
            &all_plugin_ids,
            &context.connection.config_dir_candidates,
        );
        if matches!(
            settings.persistence,
            ThemePersistence::PersistBetweenConnects
        ) {
            persist_theme_name(&context, &name);
        }
        debug!(theme = %name, "theme selected");
        return;
    }

    publish_runtime_appearance(&original_theme);
    apply_theme_extensions(
        &context,
        &original_theme,
        &all_plugin_ids,
        &context.connection.config_dir_candidates,
    );
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

fn active_theme_name(
    context: &(impl HostRuntimeApi + ?Sized),
    persistence: ThemePersistence,
    catalog: &[ThemeCatalogEntry],
    declared_name: &str,
) -> String {
    if matches!(persistence, ThemePersistence::PersistBetweenConnects)
        && let Some(name) = read_persisted_theme_name(context)
        && catalog.iter().any(|entry| entry.name == name)
    {
        return name;
    }
    if catalog.iter().any(|entry| entry.name == declared_name) {
        declared_name.to_string()
    } else {
        "default".to_string()
    }
}

fn read_persisted_theme_name(context: &(impl HostRuntimeApi + ?Sized)) -> Option<String> {
    let response = context
        .storage_get(&StorageGetRequest {
            key: STORAGE_SELECTED_APPEARANCE.to_string(),
        })
        .ok()?;
    let value = response.value?;
    String::from_utf8(value)
        .ok()
        .map(|name| normalized_theme_name(&name))
}

fn persist_theme_name(context: &impl HostRuntimeApi, name: &str) {
    let result = context.storage_set(&StorageSetRequest {
        key: STORAGE_SELECTED_APPEARANCE.to_string(),
        value: name.as_bytes().to_vec(),
    });
    if let Err(error) = result {
        warn!(%error, "failed persisting selected theme");
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
    declared_name: &str,
    active_name: &str,
) -> Vec<bmux_plugin_sdk::PromptOption> {
    catalog
        .iter()
        .map(|entry| {
            let mut label = entry.name.clone();
            if entry.name == declared_name {
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
    theme: &ThemeConfig,
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
        let Ok(payload) = bmux_plugin_sdk::encode_service_message(&request) else {
            continue;
        };
        let capability = format!("{plugin_id}.write");
        if let Err(error) = context.call_service_raw(
            &capability,
            ServiceKind::Command,
            "theme-extension",
            "apply",
            payload,
        ) {
            debug!(%error, plugin_id = %plugin_id, "theme extension apply skipped");
        }
    }
}

fn normalized_theme_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

bmux_plugin_sdk::export_plugin!(ThemePlugin, include_str!("../plugin.toml"));
