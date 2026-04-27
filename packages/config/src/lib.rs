#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Configuration management for bmux terminal multiplexer
//!
//! This crate provides configuration loading, validation, and management
//! for the bmux terminal multiplexer system.

use bmux_config_doc_derive::{ConfigDoc, ConfigDocEnum};
use bmux_event_models::Mode;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;

pub const RECORDINGS_DIR_OVERRIDE_ENV: &str = "BMUX_RECORDINGS_DIR";
pub const BMUX_CONFIG_ENV: &str = "BMUX_CONFIG";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigLoadOverrides {
    /// Optional base layer merged BELOW the primary config (lowest precedence
    /// of all layers). Typically populated by the CLI bootstrap when a slot
    /// has `inherit_base = true` and points at `<config_dir>/base.toml`.
    pub base_config_path: Option<PathBuf>,
    /// Overlay from the `BMUX_CONFIG` env var. Merged between primary and CLI.
    pub env_config_path: Option<PathBuf>,
    /// Overlay from the `--config` CLI flag. Highest precedence.
    pub cli_config_path: Option<PathBuf>,
}

impl ConfigLoadOverrides {
    #[must_use]
    pub fn from_env_with_cli(cli_config_path: Option<PathBuf>) -> Self {
        Self {
            base_config_path: None,
            env_config_path: std::env::var_os(BMUX_CONFIG_ENV).map(PathBuf::from),
            cli_config_path,
        }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.base_config_path.is_none()
            && self.env_config_path.is_none()
            && self.cli_config_path.is_none()
    }

    /// Fluent setter for [`Self::base_config_path`].
    #[must_use]
    pub fn with_base_config_path(mut self, path: Option<PathBuf>) -> Self {
        self.base_config_path = path;
        self
    }
}

pub mod keybind;
pub mod paths;

pub use bmux_config_doc::{ConfigDocSchema, FieldDoc};
pub use keybind::{KeyBindingConfig, MAX_TIMEOUT_MS, MIN_TIMEOUT_MS, ResolvedTimeout};
pub use paths::{ConfigPaths, ENV_OVERRIDE_DOCS, EnvOverrideDoc};

fn process_config_overrides() -> &'static std::sync::RwLock<Option<ConfigLoadOverrides>> {
    static OVERRIDES: std::sync::OnceLock<std::sync::RwLock<Option<ConfigLoadOverrides>>> =
        std::sync::OnceLock::new();
    OVERRIDES.get_or_init(|| std::sync::RwLock::new(None))
}

#[derive(Debug)]
pub struct ConfigOverrideGuard {
    previous: Option<ConfigLoadOverrides>,
}

impl Drop for ConfigOverrideGuard {
    fn drop(&mut self) {
        let mut guard = process_config_overrides()
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (*guard).clone_from(&self.previous);
    }
}

/// Applies process-scoped config load overrides until the returned guard is dropped.
///
/// # Panics
///
/// Panics only if lock poisoning recovery fails unexpectedly.
#[must_use]
pub fn push_process_config_overrides(overrides: ConfigLoadOverrides) -> ConfigOverrideGuard {
    let mut guard = process_config_overrides()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = guard.clone();
    *guard = Some(overrides);
    drop(guard);
    ConfigOverrideGuard { previous }
}

/// Configuration error types
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Configuration file not found: {}", path.display())]
    FileNotFound { path: PathBuf },

    #[error("Failed to read configuration file: {error}")]
    ReadError { error: String },

    #[error("Failed to parse configuration: {error}")]
    ParseError { error: String },

    #[error("Invalid configuration value: {field} = {value}")]
    InvalidValue { field: String, value: String },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct CompositionConfig {
    active_profile: Option<String>,
    layer_order: Vec<String>,
    auto_select: Vec<CompositionAutoSelectRule>,
    profiles: BTreeMap<String, CompositionProfile>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct CompositionProfile {
    extends: Vec<String>,
    patch: toml::Table,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct CompositionAutoSelectRule {
    profile: String,
    cwd_prefix: Option<String>,
    host: Option<String>,
    os: Option<String>,
    term_prefix: Option<String>,
    runtime: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompositionResolution {
    pub selected_profile: Option<String>,
    pub selected_profile_source: Option<String>,
    pub matched_auto_select_index: Option<usize>,
    pub layer_order: Vec<String>,
    pub available_profiles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompositionLayerChange {
    pub layer: String,
    pub changed_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompositionExplain {
    pub resolution: CompositionResolution,
    pub applied_layers: Vec<CompositionLayerChange>,
}

fn built_in_composition_profiles() -> BTreeMap<String, CompositionProfile> {
    fn parse_builtin_patch(profile_id: &str, source: &str) -> toml::Table {
        match toml::from_str::<toml::Table>(source) {
            Ok(table) => table,
            Err(error) => {
                panic!("invalid built-in composition profile '{profile_id}': {error}")
            }
        }
    }

    let mut profiles = BTreeMap::new();
    profiles.insert(
        "vim".to_string(),
        CompositionProfile {
            extends: vec![],
            patch: parse_builtin_patch("vim", include_str!("../profiles/vim.toml")),
        },
    );

    profiles.insert(
        "tmux_compat".to_string(),
        CompositionProfile {
            extends: vec![],
            patch: parse_builtin_patch("tmux_compat", include_str!("../profiles/tmux_compat.toml")),
        },
    );

    profiles.insert(
        "zellij_compat".to_string(),
        CompositionProfile {
            extends: vec!["vim".to_string()],
            patch: parse_builtin_patch(
                "zellij_compat",
                include_str!("../profiles/zellij_compat.toml"),
            ),
        },
    );

    profiles
}

fn canonical_profile_id(profile_id: &str) -> String {
    profile_id.trim().to_ascii_lowercase()
}

fn evaluate_auto_select_rule(rule: &CompositionAutoSelectRule) -> bool {
    if let Some(cwd_prefix) = &rule.cwd_prefix {
        let Some(cwd) = std::env::current_dir().ok() else {
            return false;
        };
        let cwd_value = cwd.to_string_lossy().to_string();
        if !cwd_value.starts_with(cwd_prefix) {
            return false;
        }
    }
    if let Some(host) = &rule.host {
        let candidate = std::env::var("HOSTNAME")
            .ok()
            .or_else(|| std::env::var("HOST").ok())
            .unwrap_or_default();
        if !candidate.eq_ignore_ascii_case(host) {
            return false;
        }
    }
    if let Some(os) = &rule.os
        && !std::env::consts::OS.eq_ignore_ascii_case(os)
    {
        return false;
    }
    if let Some(term_prefix) = &rule.term_prefix {
        let term = std::env::var("TERM").unwrap_or_default();
        if !term
            .to_ascii_lowercase()
            .starts_with(&term_prefix.to_ascii_lowercase())
        {
            return false;
        }
    }
    if let Some(runtime_name) = &rule.runtime {
        let runtime = std::env::var("BMUX_RUNTIME").unwrap_or_default();
        if !runtime.eq_ignore_ascii_case(runtime_name) {
            return false;
        }
    }
    true
}

fn merge_toml_value(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base_table), toml::Value::Table(overlay_table)) => {
            for (key, value) in overlay_table {
                if let Some(existing) = base_table.get_mut(&key) {
                    merge_toml_value(existing, value);
                } else {
                    base_table.insert(key, value);
                }
            }
        }
        (base_slot, overlay_value) => {
            *base_slot = overlay_value;
        }
    }
}

fn collect_changed_paths(
    before: Option<&toml::Value>,
    after: Option<&toml::Value>,
    prefix: &str,
    out: &mut std::collections::BTreeSet<String>,
) {
    match (before, after) {
        (None, None) => {}
        (Some(left), Some(right)) if left == right => {}
        (Some(toml::Value::Table(left)), Some(toml::Value::Table(right))) => {
            let keys = left
                .keys()
                .chain(right.keys())
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            for key in keys {
                let next_prefix = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                collect_changed_paths(left.get(&key), right.get(&key), &next_prefix, out);
            }
        }
        _ => {
            if prefix.is_empty() {
                out.insert("<root>".to_string());
            } else {
                out.insert(prefix.to_string());
            }
        }
    }
}

fn changed_paths(before: &toml::Value, after: &toml::Value) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    collect_changed_paths(Some(before), Some(after), "", &mut out);
    out.into_iter().collect()
}

fn resolve_config_override_path(value: &std::ffi::OsStr) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        return path;
    }
    std::env::current_dir().map_or_else(|_| path.clone(), |cwd| cwd.join(path.clone()))
}

fn load_toml_file(path: &std::path::Path) -> Result<toml::Value> {
    let contents = std::fs::read_to_string(path).map_err(|error| ConfigError::ReadError {
        error: format!("{} ({})", error, path.display()),
    })?;
    toml::from_str(&contents).map_err(|error| ConfigError::ParseError {
        error: format!("{} ({})", error, path.display()),
    })
}

fn merged_raw_config_value_with_overrides(
    base_path: &std::path::Path,
    explicit_overrides: Option<&ConfigLoadOverrides>,
) -> Result<Option<toml::Value>> {
    let mut source_paths = Vec::new();
    if base_path.exists() {
        source_paths.push(base_path.to_path_buf());
    }

    // Apply override layers (base / env / cli) only when:
    //   - the caller provided explicit non-empty overrides, OR
    //   - a process-scoped overrides guard is active, OR
    //   - the base_path points at the *current* default config file
    //     (preserves legacy behavior for callers that do not pass overrides).
    let process_overrides = process_config_overrides()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let has_explicit_overrides = explicit_overrides.is_some_and(|overrides| !overrides.is_empty());
    let matches_default_config_file = base_path == ConfigPaths::default().config_file();
    let include_override_layers =
        has_explicit_overrides || process_overrides.is_some() || matches_default_config_file;

    if !include_override_layers {
        let mut merged = toml::Value::Table(toml::Table::new());
        let mut loaded_any = false;
        for path in source_paths {
            let value = load_toml_file(&path)?;
            merge_toml_value(&mut merged, value);
            loaded_any = true;
        }
        return Ok(loaded_any.then_some(merged));
    }

    let resolved_overrides = explicit_overrides
        .cloned()
        .filter(|overrides| !overrides.is_empty())
        .or(process_overrides)
        .unwrap_or_else(|| ConfigLoadOverrides::from_env_with_cli(None));

    // Precedence (low → high):
    //   base_config_path → primary config → BMUX_CONFIG env → --config flag
    // Prepend the base layer so it sits below everything else. Skip the
    // base layer entirely when `BMUX_NO_BASE_CONFIG` is truthy.
    if !base_config_disabled()
        && let Some(base) = resolved_overrides.base_config_path.as_ref()
        && base.exists()
    {
        source_paths.insert(0, base.clone());
    }

    if let Some(value) = resolved_overrides.env_config_path {
        let path = resolve_config_override_path(value.as_os_str());
        if !path.exists() {
            return Err(ConfigError::FileNotFound { path });
        }
        source_paths.push(path);
    }

    if let Some(value) = resolved_overrides.cli_config_path {
        let path = resolve_config_override_path(value.as_os_str());
        if !path.exists() {
            return Err(ConfigError::FileNotFound { path });
        }
        source_paths.push(path);
    }

    let mut merged = toml::Value::Table(toml::Table::new());
    let mut loaded_any = false;
    for path in source_paths {
        let value = load_toml_file(&path)?;
        merge_toml_value(&mut merged, value);
        loaded_any = true;
    }

    Ok(loaded_any.then_some(merged))
}

fn base_config_disabled() -> bool {
    std::env::var(bmux_slots::NO_BASE_CONFIG_ENV)
        .ok()
        .is_some_and(|v| {
            let t = v.trim().to_ascii_lowercase();
            matches!(t.as_str(), "1" | "true" | "yes" | "on")
        })
}

fn apply_forced_profile(
    raw_value: &mut toml::Value,
    forced_profile: Option<&str>,
) -> std::result::Result<(), ConfigError> {
    if let Some(profile_id) = forced_profile
        && let Some(root) = raw_value.as_table_mut()
    {
        let composition_value = root
            .entry("composition".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let composition_table =
            composition_value
                .as_table_mut()
                .ok_or_else(|| ConfigError::ParseError {
                    error: "[composition] must be a table".to_string(),
                })?;
        composition_table.insert(
            "active_profile".to_string(),
            toml::Value::String(profile_id.to_string()),
        );
    }
    Ok(())
}

fn parse_composition_config(
    root: &toml::Table,
) -> std::result::Result<CompositionConfig, ConfigError> {
    let Some(value) = root.get("composition") else {
        return Ok(CompositionConfig::default());
    };
    let parsed: CompositionConfig =
        value
            .clone()
            .try_into()
            .map_err(|error| ConfigError::ParseError {
                error: format!("invalid [composition] config: {error}"),
            })?;
    Ok(parsed)
}

fn resolve_profile_patch(
    requested_profile_id: &str,
    profiles: &BTreeMap<String, CompositionProfile>,
) -> std::result::Result<toml::Table, ConfigError> {
    fn resolve_inner(
        requested_profile_id: &str,
        profiles: &BTreeMap<String, CompositionProfile>,
        stack: &mut Vec<String>,
        cache: &mut BTreeMap<String, toml::Table>,
    ) -> std::result::Result<toml::Table, ConfigError> {
        let canonical_id = canonical_profile_id(requested_profile_id);
        if let Some(resolved) = cache.get(&canonical_id) {
            return Ok(resolved.clone());
        }
        if stack.contains(&canonical_id) {
            let mut cycle = stack.clone();
            cycle.push(canonical_id.clone());
            return Err(ConfigError::InvalidValue {
                field: "composition.profiles".to_string(),
                value: format!("profile inheritance cycle detected: {}", cycle.join(" -> ")),
            });
        }

        let Some(profile) = profiles.get(&canonical_id) else {
            return Err(ConfigError::InvalidValue {
                field: "composition.active_profile".to_string(),
                value: format!(
                    "profile '{requested_profile_id}' is not defined (known profiles: {})",
                    profiles.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            });
        };

        stack.push(canonical_id.clone());
        let mut resolved = toml::Table::new();
        for parent_id in &profile.extends {
            let parent_patch = resolve_inner(parent_id, profiles, stack, cache)?;
            let mut merged = toml::Value::Table(resolved);
            merge_toml_value(&mut merged, toml::Value::Table(parent_patch));
            resolved = merged.as_table().cloned().unwrap_or_else(toml::Table::new);
        }
        let mut merged = toml::Value::Table(resolved);
        merge_toml_value(&mut merged, toml::Value::Table(profile.patch.clone()));
        let resolved = merged.as_table().cloned().unwrap_or_else(toml::Table::new);
        stack.pop();

        cache.insert(canonical_id, resolved.clone());
        Ok(resolved)
    }

    let mut stack = Vec::new();
    let mut cache = BTreeMap::new();
    resolve_inner(requested_profile_id, profiles, &mut stack, &mut cache)
}

#[allow(clippy::too_many_lines)]
// Composition resolution intentionally keeps validation and merge flow in one place
// so precedence behavior is easy to audit end-to-end.
fn resolve_composed_config_value(
    raw: &toml::Value,
) -> std::result::Result<(toml::Value, CompositionResolution), ConfigError> {
    let (resolved, resolution, _applied_layers) = resolve_composed_config_value_with_explain(raw)?;
    Ok((resolved, resolution))
}

#[allow(clippy::too_many_lines)]
// Composition resolution intentionally keeps validation and merge flow in one place
// so precedence behavior is easy to audit end-to-end.
fn resolve_composed_config_value_with_explain(
    raw: &toml::Value,
) -> std::result::Result<
    (
        toml::Value,
        CompositionResolution,
        Vec<CompositionLayerChange>,
    ),
    ConfigError,
> {
    let mut raw_table = raw
        .as_table()
        .cloned()
        .ok_or_else(|| ConfigError::ParseError {
            error: "config root must be a table".to_string(),
        })?;
    let composition = parse_composition_config(&raw_table)?;
    raw_table.remove("composition");

    let mut profiles = built_in_composition_profiles();
    for (profile_id, profile) in composition.profiles {
        let canonical_id = canonical_profile_id(&profile_id);
        if canonical_id.is_empty() {
            return Err(ConfigError::InvalidValue {
                field: "composition.profiles".to_string(),
                value: "profile id must not be empty".to_string(),
            });
        }
        profiles.insert(canonical_id, profile);
    }

    let auto_selected_profile = composition
        .auto_select
        .iter()
        .position(evaluate_auto_select_rule)
        .map(|index| {
            (
                index,
                canonical_profile_id(&composition.auto_select[index].profile),
            )
        });
    let active_profile = composition
        .active_profile
        .as_deref()
        .map(canonical_profile_id)
        .or_else(|| {
            auto_selected_profile
                .as_ref()
                .map(|(_, profile)| profile.clone())
        });

    let default_layers = if active_profile.is_some() {
        vec![
            "defaults".to_string(),
            "profile:active".to_string(),
            "config".to_string(),
        ]
    } else {
        vec!["defaults".to_string(), "config".to_string()]
    };
    let layer_order = if composition.layer_order.is_empty() {
        default_layers
    } else {
        composition.layer_order
    };

    let mut resolved = toml::Value::Table(
        toml::Value::try_from(BmuxConfig::default())
            .map_err(|error| ConfigError::ParseError {
                error: format!("failed to serialize default config: {error}"),
            })?
            .as_table()
            .cloned()
            .unwrap_or_else(toml::Table::new),
    );
    let mut applied_layers = Vec::new();

    for layer in &layer_order {
        let before = resolved.clone();
        match layer.as_str() {
            "defaults" => {}
            "config" => {
                merge_toml_value(&mut resolved, toml::Value::Table(raw_table.clone()));
            }
            "profile:active" => {
                let Some(active_profile) = active_profile.as_deref() else {
                    return Err(ConfigError::InvalidValue {
                        field: "composition.layer_order".to_string(),
                        value: "layer 'profile:active' requires composition.active_profile"
                            .to_string(),
                    });
                };
                let patch = resolve_profile_patch(active_profile, &profiles)?;
                merge_toml_value(&mut resolved, toml::Value::Table(patch));
            }
            _ if layer.starts_with("profile:") => {
                let profile_id = layer.trim_start_matches("profile:");
                let patch = resolve_profile_patch(profile_id, &profiles)?;
                merge_toml_value(&mut resolved, toml::Value::Table(patch));
            }
            unknown => {
                return Err(ConfigError::InvalidValue {
                    field: "composition.layer_order".to_string(),
                    value: format!("unknown layer '{unknown}'"),
                });
            }
        }

        applied_layers.push(CompositionLayerChange {
            layer: layer.clone(),
            changed_paths: changed_paths(&before, &resolved),
        });
    }

    let mut available_profiles = profiles.keys().cloned().collect::<Vec<_>>();
    available_profiles.sort();
    let resolution = CompositionResolution {
        selected_profile: active_profile,
        selected_profile_source: if composition.active_profile.is_some() {
            Some("composition.active_profile".to_string())
        } else if auto_selected_profile.is_some() {
            Some("composition.auto_select".to_string())
        } else {
            None
        },
        matched_auto_select_index: auto_selected_profile.map(|(index, _)| index),
        layer_order,
        available_profiles,
    };

    Ok((resolved, resolution, applied_layers))
}

/// Root configuration structure for bmux, deserialized from `bmux.toml`
#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[serde(default)]
pub struct BmuxConfig {
    /// Core session defaults: shell, scrollback depth, and server connection settings
    #[config_doc(nested)]
    pub general: GeneralConfig,
    /// Visual styling: pane borders, status bar placement, and window titles
    #[config_doc(nested)]
    pub appearance: AppearanceConfig,
    /// Runtime behavior toggles for terminal protocol handling, layout persistence, and build compatibility
    #[config_doc(nested)]
    pub behavior: BehaviorConfig,
    /// Settings for multiple clients attached to the same session
    #[config_doc(nested)]
    pub multi_client: MultiClientConfig,
    /// Keyboard shortcuts organized by scope and interaction mode
    #[config_doc(nested)]
    pub keybindings: KeyBindingConfig,
    /// Plugin discovery, enablement, and per-plugin settings
    #[config_doc(nested)]
    pub plugins: PluginConfig,
    /// Local and remote connection target profiles
    #[config_doc(nested)]
    pub connections: ConnectionsConfig,
    /// Content and layout of the status bar displayed at the top or bottom of the terminal
    #[config_doc(nested)]
    pub status_bar: StatusBarConfig,
    /// Session recording for terminal replay, debugging, and playbook generation
    #[config_doc(nested)]
    pub recording: RecordingConfig,
    /// Performance diagnostics capture controls and telemetry safety limits
    #[config_doc(nested)]
    pub performance: PerformanceConfig,
    /// Kiosk profiles and SSH/bootstrap settings for locked-down access flows
    #[config_doc(nested)]
    pub kiosk: KioskConfig,
    /// Sandbox workflow defaults for cleanup and isolation operations
    #[config_doc(nested)]
    pub sandbox: SandboxConfig,
}

/// Kiosk profile configuration for SSH-first locked sessions.
#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[config_doc(section = "kiosk")]
#[serde(default)]
pub struct KioskConfig {
    /// Shared defaults inherited by all kiosk profiles.
    #[config_doc(nested)]
    pub defaults: KioskDefaultsConfig,
    /// Named kiosk profile overrides.
    #[config_doc(nested, map_key = "<name>")]
    pub profiles: BTreeMap<String, KioskProfileConfig>,
    /// Optional file output locations used by `bmux kiosk init`.
    #[config_doc(nested)]
    pub files: KioskFilesConfig,
}

/// Shared kiosk defaults.
#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[serde(default)]
pub struct KioskDefaultsConfig {
    /// Enable kiosk features and commands.
    pub enabled: bool,
    /// Default SSH user when profiles do not override it.
    pub ssh_user: String,
    /// Default role assigned to issued kiosk tokens.
    pub role: KioskRole,
    /// Allow detach in kiosk attach mode.
    pub allow_detach: bool,
    /// Default issued token TTL in seconds.
    pub token_ttl_secs: u64,
    /// Require one-time token usage by default.
    pub one_shot: bool,
    /// Sandbox mode preference.
    pub sandbox: KioskSandboxMode,
}

impl Default for KioskDefaultsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ssh_user: "bmux-kiosk".to_string(),
            role: KioskRole::Observer,
            allow_detach: false,
            token_ttl_secs: 15 * 60,
            one_shot: true,
            sandbox: KioskSandboxMode::Auto,
        }
    }
}

/// Per-profile kiosk overrides.
#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[serde(default)]
pub struct KioskProfileConfig {
    /// Optional session name to attach when token omits a session override.
    pub session: Option<String>,
    /// Optional target name for attach routing.
    pub target: Option<String>,
    /// Override role for this profile.
    pub role: Option<KioskRole>,
    /// Override SSH user for this profile.
    pub ssh_user: Option<String>,
    /// Override detach policy for this profile.
    pub allow_detach: Option<bool>,
    /// Override token TTL in seconds for this profile.
    pub token_ttl_secs: Option<u64>,
    /// Override one-shot token behavior for this profile.
    pub one_shot: Option<bool>,
    /// Override sandbox mode for this profile.
    pub sandbox: Option<KioskSandboxMode>,
}

/// File output locations for generated kiosk assets.
#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[serde(default)]
pub struct KioskFilesConfig {
    /// Destination path for generated sshd include content.
    pub sshd_include_path: Option<PathBuf>,
    /// Destination directory for generated wrapper scripts.
    pub wrapper_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum KioskRole {
    #[default]
    Observer,
    Writer,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum KioskSandboxMode {
    #[default]
    Auto,
    None,
    Container,
    Native,
}

/// Sandbox workflow defaults for cleanup and isolation operations.
#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[config_doc(section = "sandbox")]
#[serde(default)]
pub struct SandboxConfig {
    /// Cleanup policy defaults used by `bmux sandbox cleanup`.
    #[config_doc(nested)]
    pub cleanup: SandboxCleanupConfig,
}

/// Cleanup policy defaults for sandbox maintenance commands.
#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "sandbox.cleanup")]
#[serde(default)]
pub struct SandboxCleanupConfig {
    /// When true, cleanup removes only failed/aborted sandboxes.
    pub failed_only: bool,
    /// Minimum age in seconds before sandbox is eligible for cleanup.
    pub older_than_secs: u64,
    /// Default cleanup source scope.
    pub source: SandboxCleanupSource,
}

impl Default for SandboxCleanupConfig {
    fn default() -> Self {
        Self {
            failed_only: false,
            older_than_secs: 300,
            source: SandboxCleanupSource::All,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum SandboxCleanupSource {
    SandboxCli,
    Playbook,
    RecordingVerify,
    #[default]
    All,
}

/// Local and remote connection target profiles used by `bmux connect` and
/// global command targeting.
#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[config_doc(section = "connections")]
#[serde(default)]
pub struct ConnectionsConfig {
    /// Hosted mode strategy. `p2p` avoids control-plane dependencies by default.
    pub hosted_mode: HostedMode,
    /// Default command target when `--target` is not passed.
    pub default_target: Option<String>,
    /// Optional control-plane base URL for hosted auth/share resolution.
    pub control_plane_url: Option<String>,
    /// Named connection targets.
    #[config_doc(nested, map_key = "<name>")]
    pub targets: BTreeMap<String, ConnectionTargetConfig>,
    /// Most recently used targets (newest first).
    pub recent_targets: Vec<String>,
    /// Most recently used sessions per target (newest first).
    pub recent_sessions: BTreeMap<String, Vec<String>>,
    /// User-defined share links (bmux://<name> -> target reference)
    pub share_links: BTreeMap<String, String>,
    /// Optional SSH-key allowlist and enforcement for iroh connections.
    #[config_doc(nested)]
    pub iroh_ssh_access: IrohSshAccessConfig,
}

/// SSH-key allowlist configuration for iroh access control.
#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[serde(default)]
pub struct IrohSshAccessConfig {
    /// Require SSH challenge authentication on iroh connections.
    pub enabled: bool,
    /// Fingerprint -> key metadata map.
    #[config_doc(nested, map_key = "<fingerprint>")]
    pub allowlist: BTreeMap<String, IrohSshAuthorizedKey>,
}

/// Authorized SSH public key entry.
#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[serde(default)]
pub struct IrohSshAuthorizedKey {
    /// Public key in OpenSSH one-line format.
    pub public_key: String,
    /// Optional descriptive label (for display and management UX).
    pub label: Option<String>,
    /// Unix timestamp of when the key was added.
    pub added_at_unix: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum HostedMode {
    #[default]
    P2p,
    ControlPlane,
}

#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[serde(default)]
pub struct ConnectionTargetConfig {
    /// Transport backend for this target.
    pub transport: ConnectionTransport,
    /// SSH host for `transport = "ssh"`.
    pub host: Option<String>,
    /// SSH username override.
    pub user: Option<String>,
    /// SSH port override.
    pub port: Option<u16>,
    /// Private key file path.
    /// Supports `~`, `$VAR`, and `${VAR}` interpolation.
    pub identity_file: Option<PathBuf>,
    /// Known hosts file path.
    /// Supports `~`, `$VAR`, and `${VAR}` interpolation.
    pub known_hosts_file: Option<PathBuf>,
    /// Optional CA certificate bundle used for TLS transport.
    /// Supports `~`, `$VAR`, and `${VAR}` interpolation.
    pub ca_file: Option<PathBuf>,
    /// Optional TLS server name override (defaults to host).
    pub server_name: Option<String>,
    /// Iroh endpoint id for hosted transport.
    pub endpoint_id: Option<String>,
    /// Optional Iroh relay URL for hosted transport.
    pub relay_url: Option<String>,
    /// Require SSH auth handshake when connecting to this iroh target.
    pub iroh_ssh_auth: bool,
    /// Require strict host key checking.
    pub strict_host_key_checking: bool,
    /// SSH jump host (`ProxyJump`) value.
    pub jump: Option<String>,
    /// Remote bmux executable path.
    pub remote_bmux_path: String,
    /// Connection timeout in milliseconds.
    pub connect_timeout_ms: u64,
    /// Remote server startup behavior.
    pub server_start_mode: RemoteServerStartMode,
    /// Default session name used by clients.
    pub default_session: Option<String>,
}

impl Default for ConnectionTargetConfig {
    fn default() -> Self {
        Self {
            transport: ConnectionTransport::Local,
            host: None,
            user: None,
            port: None,
            identity_file: None,
            known_hosts_file: None,
            ca_file: None,
            server_name: None,
            endpoint_id: None,
            relay_url: None,
            iroh_ssh_auth: false,
            strict_host_key_checking: true,
            jump: None,
            remote_bmux_path: "bmux".to_string(),
            connect_timeout_ms: 8_000,
            server_start_mode: RemoteServerStartMode::Auto,
            default_session: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionTransport {
    #[default]
    Local,
    Ssh,
    Tls,
    Iroh,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum RemoteServerStartMode {
    #[default]
    Auto,
    RequireRunning,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum RecordingEventKindConfig {
    PaneInputRaw,
    PaneOutputRaw,
    ProtocolReplyRaw,
    PaneImage,
    ServerEvent,
    RequestStart,
    RequestDone,
    RequestError,
    Custom,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceRecordingLevel {
    #[default]
    Off,
    Basic,
    Detailed,
    Trace,
}

/// Performance diagnostics capture settings.
#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "performance")]
#[serde(default)]
pub struct PerformanceConfig {
    /// Capture verbosity for performance telemetry written into recording custom events.
    pub recording_level: PerformanceRecordingLevel,
    /// Aggregation window for periodic performance telemetry, in milliseconds.
    pub window_ms: u64,
    /// Maximum number of performance custom events emitted per second by a client/runtime.
    pub max_events_per_sec: u32,
    /// Maximum serialized performance payload bytes emitted per second by a client/runtime.
    pub max_payload_bytes_per_sec: usize,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            recording_level: PerformanceRecordingLevel::Off,
            window_ms: 1000,
            max_events_per_sec: 32,
            max_payload_bytes_per_sec: 64 * 1024,
        }
    }
}

/// Session recording for terminal replay, debugging, and playbook generation.
///
/// Records pane I/O and lifecycle events to disk.
#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "recording")]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct RecordingConfig {
    /// Root directory for recording data.
    /// Supports `~`, `$VAR`, and `${VAR}` interpolation.
    /// Relative paths are resolved against the directory containing `bmux.toml`.
    pub dir: Option<PathBuf>,
    /// Enable hidden rolling recording by default when the server starts.
    ///
    /// Manual recordings (`bmux recording start/stop`) remain available even
    /// when this is false.
    pub enabled: bool,
    /// Capture pane input bytes (keystrokes sent to pane processes)
    pub capture_input: bool,
    /// Capture pane output bytes (terminal output from pane processes)
    pub capture_output: bool,
    /// Capture lifecycle and server events (pane creation, resize, close, etc.)
    pub capture_events: bool,
    /// Override rolling capture of pane input bytes.
    ///
    /// `None` falls back to `recording.capture_input`.
    pub rolling_capture_input: Option<bool>,
    /// Override rolling capture of pane output bytes.
    ///
    /// `None` falls back to `recording.capture_output`.
    pub rolling_capture_output: Option<bool>,
    /// Override rolling capture of lifecycle/request/custom events.
    ///
    /// `None` falls back to `recording.capture_events`.
    pub rolling_capture_events: Option<bool>,
    /// Enable rolling capture of protocol reply bytes.
    ///
    /// `None` defaults to `false`.
    pub rolling_capture_protocol_replies: Option<bool>,
    /// Enable rolling capture of extracted pane image payloads.
    ///
    /// `None` defaults to `false`.
    pub rolling_capture_images: Option<bool>,
    /// Explicit rolling event-kind allowlist. When non-empty, this takes
    /// precedence over rolling capture category booleans.
    pub rolling_event_kinds: Vec<RecordingEventKindConfig>,
    /// Rotate recording segments at approximately this size in MB
    pub segment_mb: usize,
    /// Retention period for completed recordings in days. Set to 0 to disable
    /// automatic pruning and keep recordings indefinitely.
    pub retention_days: u64,
    /// Enable hidden always-on rolling capture and retain at most this many
    /// seconds of recent events. Set to 0 to disable rolling capture.
    pub rolling_window_secs: u64,
    /// Automatically export stopped/cut user-initiated recordings as GIF.
    pub auto_export: bool,
    /// Directory for auto-exported GIFs.
    ///
    /// Supports `~`, `$VAR`, and `${VAR}` interpolation.
    /// Relative paths are resolved against the directory containing `bmux.toml`.
    /// When unset, GIFs are written next to the recording directory.
    pub auto_export_dir: Option<PathBuf>,
    /// Default settings for `recording export` rendering.
    #[config_doc(nested)]
    pub export: RecordingExportConfig,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            dir: None,
            enabled: true,
            capture_input: true,
            capture_output: true,
            capture_events: true,
            rolling_capture_input: None,
            rolling_capture_output: None,
            rolling_capture_events: None,
            rolling_capture_protocol_replies: None,
            rolling_capture_images: None,
            rolling_event_kinds: Vec::new(),
            segment_mb: 64,
            retention_days: 30,
            rolling_window_secs: 0,
            auto_export: false,
            auto_export_dir: None,
            export: RecordingExportConfig::default(),
        }
    }
}

/// Defaults for `recording export` cursor rendering behavior.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ConfigDoc)]
#[config_doc(section = "recording.export")]
#[serde(default)]
pub struct RecordingExportConfig {
    /// Cursor rendering default for `recording export`.
    pub cursor: RecordingExportCursorMode,
    /// Cursor shape default for `recording export`.
    pub cursor_shape: RecordingExportCursorShape,
    /// Cursor blink default for `recording export`.
    pub cursor_blink: RecordingExportCursorBlinkMode,
    /// Cursor blink period default for `recording export`.
    pub cursor_blink_period_ms: u32,
    /// Cursor color default for `recording export` (`auto` or #RRGGBB).
    pub cursor_color: String,
    /// Cursor behavior profile default for `recording export`.
    pub cursor_profile: RecordingExportCursorProfile,
    /// Keep cursor solid after activity for this duration in milliseconds.
    /// `None` lets terminal profiles choose an emulator-specific default.
    pub cursor_solid_after_activity_ms: Option<u32>,
    /// Keep cursor solid after input activity for this duration in milliseconds.
    pub cursor_solid_after_input_ms: Option<u32>,
    /// Keep cursor solid after output activity for this duration in milliseconds.
    pub cursor_solid_after_output_ms: Option<u32>,
    /// Keep cursor solid after cursor movement activity for this duration in milliseconds.
    pub cursor_solid_after_cursor_ms: Option<u32>,
    /// Cursor paint mode default for `recording export`.
    pub cursor_paint_mode: RecordingExportCursorPaintMode,
    /// Cursor text mode default for `recording export`.
    pub cursor_text_mode: RecordingExportCursorTextMode,
    /// Cursor bar width default as a percent of cell width.
    pub cursor_bar_width_pct: u8,
    /// Cursor underline height default as a percent of cell height.
    pub cursor_underline_height_pct: u8,
    /// Palette source default for `recording export`.
    pub palette_source: RecordingExportPaletteSource,
    /// Default foreground override for `recording export` palette resolution.
    ///
    /// Accepts `auto`/empty to keep source-derived defaults, or a color value.
    pub palette_foreground: Option<String>,
    /// Default background override for `recording export` palette resolution.
    ///
    /// Accepts `auto`/empty to keep source-derived defaults, or a color value.
    pub palette_background: Option<String>,
    /// Default indexed color overrides for `recording export`.
    ///
    /// Entries must be `INDEX=COLOR` (for example `5=#bb78d9`).
    pub palette_colors: Vec<String>,
}

impl Default for RecordingExportConfig {
    fn default() -> Self {
        Self {
            cursor: RecordingExportCursorMode::Auto,
            cursor_shape: RecordingExportCursorShape::Auto,
            cursor_blink: RecordingExportCursorBlinkMode::Auto,
            cursor_blink_period_ms: 500,
            cursor_color: "auto".to_string(),
            cursor_profile: RecordingExportCursorProfile::Auto,
            cursor_solid_after_activity_ms: None,
            cursor_solid_after_input_ms: None,
            cursor_solid_after_output_ms: None,
            cursor_solid_after_cursor_ms: None,
            cursor_paint_mode: RecordingExportCursorPaintMode::Auto,
            cursor_text_mode: RecordingExportCursorTextMode::Auto,
            cursor_bar_width_pct: 16,
            cursor_underline_height_pct: 12,
            palette_source: RecordingExportPaletteSource::Auto,
            palette_foreground: None,
            palette_background: None,
            palette_colors: Vec::new(),
        }
    }
}

impl RecordingExportConfig {
    #[must_use]
    pub const fn config_doc_values() -> &'static [&'static str] {
        &[]
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ConfigDocEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingExportCursorMode {
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ConfigDocEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingExportCursorShape {
    Auto,
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ConfigDocEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingExportCursorBlinkMode {
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ConfigDocEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingExportCursorProfile {
    Auto,
    Ghostty,
    Generic,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ConfigDocEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingExportCursorPaintMode {
    Auto,
    Invert,
    Fill,
    Outline,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ConfigDocEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingExportCursorTextMode {
    Auto,
    SwapFgBg,
    ForceContrast,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ConfigDocEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingExportPaletteSource {
    Auto,
    Recording,
    Terminal,
    Xterm,
}

/// Core session defaults: shell, scrollback depth, and server connection settings
#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "general")]
#[serde(default)]
pub struct GeneralConfig {
    /// Default interaction mode when starting bmux
    #[config_doc(values("normal", "insert", "visual", "command"))]
    pub default_mode: Mode,
    /// Enable mouse support for clicking to focus panes, scrolling through
    /// output, and dragging to resize pane borders
    pub mouse_support: bool,
    /// Default shell to launch in new panes. When unset, uses the SHELL
    /// environment variable or falls back to /bin/sh.
    pub default_shell: Option<String>,
    /// Maximum number of scrollback lines retained per pane. Must be at least 1.
    pub scrollback_limit: usize,
    /// Server socket timeout in milliseconds. Must be at least 1.
    pub server_timeout: u64,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            default_mode: Mode::Normal,
            mouse_support: true,
            default_shell: None,
            scrollback_limit: 10_000,
            server_timeout: 5_000,
        }
    }
}

/// Visual styling: pane borders, status bar placement, and window titles
#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[config_doc(section = "appearance")]
#[serde(default)]
pub struct AppearanceConfig {
    /// Where to render the status bar. TOP places it above panes, BOTTOM below
    /// panes, and OFF hides it entirely.
    pub status_position: StatusPosition,
    /// Drawing style for pane borders. NONE hides borders entirely, giving
    /// panes the full terminal width.
    pub pane_border_style: BorderStyle,
    /// Display a title label in each pane's border showing the running
    /// process name
    pub show_pane_titles: bool,
    /// Format string for the outer terminal's title bar. Empty string leaves
    /// the title unset.
    pub window_title_format: String,
}

/// Runtime behavior toggles for terminal protocol handling, layout persistence,
/// and build compatibility
#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "behavior")]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct BehaviorConfig {
    /// Immediately resize panes to the largest remaining client when a client
    /// disconnects, rather than keeping the previous dimensions
    pub aggressive_resize: bool,
    /// Highlight panes in the status bar when they produce output while
    /// unfocused, so you can tell which panes have new activity
    pub visual_activity: bool,
    /// How to handle terminal bell signals from panes. NONE ignores bells
    /// entirely. ANY notifies on bells from any pane. CURRENT only notifies
    /// from the focused pane. OTHER only notifies from unfocused panes.
    pub bell_action: BellAction,
    /// Automatically rename windows based on the currently running command
    /// in the focused pane
    pub automatic_rename: bool,
    /// Exit bmux when no sessions remain
    pub exit_empty: bool,
    /// Restore and persist the last local CLI runtime layout across sessions,
    /// so reattaching resumes where you left off
    pub restore_last_layout: bool,
    /// Prompt for confirmation before a destructive quit that clears
    /// persisted local runtime state
    pub confirm_quit_destroy: bool,
    /// Terminal type exposed to pane processes via the TERM environment
    /// variable. Common values: bmux-256color, xterm-256color, screen-256color.
    pub pane_term: String,
    /// Enable per-shell runtime integration hooks that emit verbatim pane
    /// command and prompt metadata for resurrection.
    ///
    /// When disabled, bmux does not inject shell wrapper config/rc files and
    /// falls back to best-effort process inspection for command/cwd restore.
    pub pane_shell_integration: bool,
    /// Enable protocol query/reply tracing in the runtime. Useful for
    /// debugging terminal protocol behavior with CSI/OSC/DCS sequences.
    pub protocol_trace_enabled: bool,
    /// Maximum number of in-memory protocol trace events to retain.
    /// Must be at least 1.
    pub protocol_trace_capacity: usize,
    /// What to do when the bmux terminfo entry is missing from the system.
    /// ask prompts before installing. always installs silently. never skips
    /// installation, which may degrade terminal rendering.
    pub terminfo_auto_install: TerminfoAutoInstall,
    /// Number of days to wait before prompting again after the user declines
    /// terminfo installation
    pub terminfo_prompt_cooldown_days: u64,
    /// What to do when the running server was built from a different version
    /// than the current CLI binary. ignore skips stale-build checks. warn
    /// connects with a warning message. error refuses to connect until the
    /// server is restarted.
    pub stale_build_action: StaleBuildAction,
    /// Enable the Kitty keyboard protocol for enhanced key reporting.
    /// When true, bmux negotiates enhanced keyboard mode with the outer
    /// terminal, allowing modified special keys like Ctrl+Enter to be
    /// correctly forwarded to pane programs.
    pub kitty_keyboard: bool,
    /// How to restore pane content when hidden panes become visible again
    /// (e.g. after exiting zoom). SNAPSHOT re-fetches from the server for
    /// guaranteed accuracy. RETAIN keeps parsers in memory for instant restore.
    pub pane_restore_method: PaneRestoreMethod,
    /// Mouse interaction settings for attach mode (focus/scroll gestures).
    #[config_doc(nested)]
    pub mouse: MouseBehaviorConfig,
    /// Terminal image protocol settings (Sixel, Kitty graphics, iTerm2).
    #[config_doc(nested)]
    pub images: ImageBehaviorConfig,
    /// IPC compression settings (image payloads and remote connections).
    #[config_doc(nested)]
    pub compression: CompressionConfig,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            aggressive_resize: false,
            visual_activity: false,
            bell_action: BellAction::Any,
            automatic_rename: false,
            exit_empty: false,
            restore_last_layout: true,
            confirm_quit_destroy: true,
            pane_term: "bmux-256color".to_string(),
            pane_shell_integration: true,
            protocol_trace_enabled: false,
            protocol_trace_capacity: 200,
            terminfo_auto_install: TerminfoAutoInstall::Never,
            terminfo_prompt_cooldown_days: 7,
            stale_build_action: StaleBuildAction::Ignore,
            kitty_keyboard: true,
            pane_restore_method: PaneRestoreMethod::Snapshot,
            mouse: MouseBehaviorConfig::default(),
            images: ImageBehaviorConfig::default(),
            compression: CompressionConfig::default(),
        }
    }
}

/// Mouse interaction behavior for attach mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ConfigDoc)]
#[config_doc(section = "behavior.mouse")]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct MouseBehaviorConfig {
    /// Master toggle for mouse handling in attach mode.
    pub enabled: bool,
    /// Focus pane when clicking inside it.
    pub focus_on_click: bool,
    /// How pane-area mouse clicks are routed between bmux and pane TUIs.
    pub click_propagation: MouseClickPropagation,
    /// Focus pane when hovering over it.
    pub focus_on_hover: bool,
    /// Hover dwell time before focus is applied.
    pub hover_delay_ms: u64,
    /// Route wheel scrolling to focused pane scrollback.
    pub scroll_scrollback: bool,
    /// How wheel events are routed between pane TUIs and bmux scrollback.
    pub wheel_propagation: MouseWheelPropagation,
    /// Number of scrollback lines per mouse wheel tick.
    pub scroll_lines_per_tick: u16,
    /// Exit scrollback mode automatically when wheel scrolling reaches bottom.
    pub exit_scrollback_on_bottom: bool,
    /// Resize panes by dragging shared pane borders.
    pub resize_borders: bool,
    /// Optional mouse gesture overrides mapped to action names.
    ///
    /// Supported keys in the current core runtime are `click_left`,
    /// `hover_focus`, `scroll_up`, and `scroll_down`.
    /// Values use the same action naming format as keybindings, including
    /// built-in action names and `plugin:<id>:<command>`.
    pub gesture_actions: BTreeMap<String, String>,
}

impl Default for MouseBehaviorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            focus_on_click: true,
            click_propagation: MouseClickPropagation::default(),
            focus_on_hover: false,
            hover_delay_ms: 175,
            scroll_scrollback: true,
            wheel_propagation: MouseWheelPropagation::default(),
            scroll_lines_per_tick: 3,
            exit_scrollback_on_bottom: true,
            resize_borders: true,
            gesture_actions: BTreeMap::new(),
        }
    }
}

/// Terminal image protocol behavior.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ConfigDoc)]
#[config_doc(section = "behavior.images")]
#[serde(default)]
pub struct ImageBehaviorConfig {
    /// Master switch for Sixel, Kitty graphics, and iTerm2 inline image
    /// support.  When false, image escape sequences are passed through to
    /// the host terminal without interception or registry tracking.
    pub enabled: bool,
    /// How image decoding is distributed between server and client.
    /// PASSTHROUGH forwards raw protocol bytes with coordinate translation
    /// (fastest, requires same protocol support on the host terminal).
    /// SERVER decodes images to pixel buffers on the server side.
    /// CLIENT sends raw bytes for the client to decode and re-encode.
    pub decode_mode: ImageDecodeMode,
    /// Maximum image payload size in bytes per image.  Images exceeding
    /// this limit are silently discarded to prevent memory exhaustion.
    pub max_image_bytes: u64,
    /// Maximum number of images kept in the registry per pane.  When this
    /// limit is reached, the oldest images are evicted (FIFO).
    pub max_images_per_pane: u32,
}

impl Default for ImageBehaviorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            decode_mode: ImageDecodeMode::Passthrough,
            max_image_bytes: 10 * 1024 * 1024, // 10 MiB
            max_images_per_pane: 100,
        }
    }
}

impl ImageBehaviorConfig {
    /// Config doc support — empty since this is a nested struct, not an enum.
    #[must_use]
    pub const fn config_doc_values() -> &'static [&'static str] {
        &[]
    }
}

/// How image decoding is distributed between server and client.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum ImageDecodeMode {
    /// Server decodes images to pixel buffers; client encodes for host protocol.
    Server,
    /// Server sends raw protocol bytes; client decodes + re-encodes.
    Client,
    /// Raw bytes forwarded with coordinate translation (same-protocol only).
    #[default]
    Passthrough,
}

/// IPC compression settings.
///
/// Controls two user-facing compression layers:
/// - **images**: Per-image payload compression (zstd/lz4 on raw image data
///   before IPC transport).
/// - **remote**: Stream-level compression for remote connections (TLS gateway,
///   Iroh P2P).  Local Unix socket connections are never compressed.
///
/// Default is `enabled = true`, `images = auto`, `remote = auto`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ConfigDoc)]
#[config_doc(section = "behavior.compression")]
#[serde(default)]
pub struct CompressionConfig {
    /// Master switch for all compression.  When false, image payloads are
    /// sent uncompressed and remote connections are not wrapped in streaming
    /// compression.
    pub enabled: bool,
    /// Compress image payloads (Sixel, Kitty graphics, iTerm2) before IPC
    /// transport.  Typical reduction: 5-15x for sixel text, 3-20x for kitty
    /// raw pixels.  Pre-compressed formats (e.g. kitty PNG) are automatically
    /// detected and skipped.  `auto` selects zstd when available.
    pub images: CompressionMode,
    /// Compress remote connections (TLS gateway, Iroh P2P) with streaming
    /// compression.  Local Unix socket connections are never compressed.
    /// Both the client and the server gateway must use the same setting;
    /// a mismatch will cause the connection to fail.  If SSH is already
    /// compressing the tunnel, set this to `none` to avoid double work.
    pub remote: CompressionMode,
    /// Zstd compression level for image payloads (1-19, ignored for lz4).
    /// Level 1 is fastest (~500 MB/s), level 3 (default) balances speed and
    /// ratio, level 9+ gives diminishing returns.  Remote streaming always
    /// uses level 1 internally for low latency regardless of this setting.
    pub level: i32,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            images: CompressionMode::Auto,
            remote: CompressionMode::Auto,
            level: 3,
        }
    }
}

impl CompressionConfig {
    /// Config doc support.
    #[must_use]
    pub const fn config_doc_values() -> &'static [&'static str] {
        &[]
    }
}

/// Compression algorithm selection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum CompressionMode {
    /// No compression.
    None,
    /// Select the best available algorithm automatically.
    #[default]
    Auto,
    /// Use zstd compression.
    Zstd,
    /// Use lz4 compression.
    Lz4,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum MouseClickPropagation {
    FocusOnly,
    ForwardOnly,
    #[default]
    FocusAndForward,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum MouseWheelPropagation {
    #[default]
    ForwardOnly,
    ScrollbackOnly,
    ForwardAndScrollback,
}

impl MouseBehaviorConfig {
    #[must_use]
    pub const fn config_doc_values() -> &'static [&'static str] {
        &[]
    }

    #[must_use]
    pub const fn effective_click_propagation(&self) -> MouseClickPropagation {
        if !self.focus_on_click {
            return MouseClickPropagation::ForwardOnly;
        }
        self.click_propagation
    }

    #[must_use]
    pub const fn effective_wheel_propagation(&self) -> MouseWheelPropagation {
        match (self.wheel_propagation, self.scroll_scrollback) {
            (
                MouseWheelPropagation::ScrollbackOnly | MouseWheelPropagation::ForwardAndScrollback,
                false,
            ) => MouseWheelPropagation::ForwardOnly,
            (other, _) => other,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum StaleBuildAction {
    #[default]
    Ignore,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum TerminfoAutoInstall {
    Ask,
    Always,
    #[default]
    Never,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum PaneRestoreMethod {
    /// Re-fetch full pane content from the server ring buffer when panes
    /// become visible again (e.g. after unzoom). Most robust — always correct.
    #[default]
    Snapshot,
    /// Keep hidden pane terminal parsers alive in client memory. Instant
    /// restore with no network cost, but content may appear briefly stale
    /// until incremental output catches up.
    Retain,
}

/// Settings for multiple clients attached to the same session, controlling
/// independent views and mode synchronization
#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[config_doc(section = "multi_client")]
#[serde(default)]
pub struct MultiClientConfig {
    /// Allow clients to have independent views of the same session, with
    /// separate focused panes and scroll positions
    pub allow_independent_views: bool,
    /// When true, new clients automatically track the leader client's focused
    /// pane and scroll position. When false, clients start with an independent view.
    pub default_follow_mode: bool,
    /// Maximum number of clients that can attach to a single session.
    /// Set to 0 for unlimited.
    pub max_clients_per_session: usize,
    /// When true, switching interaction modes (e.g. Normal to Insert) on one
    /// client applies to all attached clients in the same session
    pub sync_client_modes: bool,
}

/// Plugin discovery, enablement, and per-plugin settings. Bundled plugins
/// (like bmux.windows and bmux.permissions) are enabled by default.
#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[config_doc(section = "plugins")]
#[serde(default)]
pub struct PluginConfig {
    /// Plugin IDs to enable in addition to the bundled defaults. Bundled
    /// plugins like bmux.windows and bmux.permissions are enabled automatically
    /// without being listed here.
    pub enabled: Vec<String>,
    /// Plugin IDs to explicitly disable, including bundled ones. Overrides
    /// both bundled defaults and the enabled list.
    pub disabled: Vec<String>,
    /// Additional directories to scan for plugin binaries beyond the default
    /// plugin search path.
    /// Supports `~`, `$VAR`, and `${VAR}` interpolation.
    pub search_paths: Vec<PathBuf>,
    /// Per-plugin settings keyed by plugin ID. Each plugin defines its own
    /// accepted keys and values.
    pub settings: BTreeMap<String, toml::Value>,
    /// Command routing policy for plugin ownership and startup validation.
    #[config_doc(nested)]
    pub routing: PluginRoutingPolicyConfig,
}

/// Command routing policy for plugin CLI ownership.
#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[serde(default)]
pub struct PluginRoutingPolicyConfig {
    /// Conflict behavior when multiple plugins claim overlapping command ownership.
    pub conflict_mode: PluginRoutingConflictMode,
    /// Required namespace claims that must be satisfied at startup.
    #[config_doc(nested, list_index = "<index>")]
    pub required_namespaces: Vec<RequiredNamespaceClaim>,
    /// Required path claims that must be satisfied at startup.
    #[config_doc(nested, list_index = "<index>")]
    pub required_paths: Vec<RequiredPathClaim>,
}

impl PluginRoutingPolicyConfig {
    #[must_use]
    pub const fn config_doc_values() -> &'static [&'static str] {
        &[]
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, ConfigDocEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginRoutingConflictMode {
    #[default]
    FailStartup,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[serde(default)]
pub struct RequiredNamespaceClaim {
    /// Namespace segment that must be owned by a plugin.
    pub namespace: String,
    /// Optional owner plugin ID; when omitted, any plugin may own the namespace.
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, ConfigDoc)]
#[serde(default)]
pub struct RequiredPathClaim {
    /// Command path that must be owned by a plugin.
    pub path: Vec<String>,
    /// Optional owner plugin ID; when omitted, any plugin may own the path.
    pub owner: Option<String>,
}

/// Content and layout of the status bar displayed at the top or bottom
/// of the terminal.
#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "status_bar")]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct StatusBarConfig {
    /// Enable status bar rendering in attach UI.
    pub enabled: bool,
    /// High-level status bar visual preset.
    pub preset: StatusBarPreset,
    /// Layout and spacing options.
    #[config_doc(nested)]
    pub layout: StatusBarLayoutConfig,
    /// Separator and emphasis options.
    #[config_doc(nested)]
    pub style: StatusBarStyleConfig,
    /// Optional status-specific color overrides.
    #[config_doc(nested)]
    pub colors: StatusBarColorConfig,
    /// Maximum number of tabs shown in the tab strip before overflow is collapsed.
    pub max_tabs: usize,
    /// Maximum display width for each tab label.
    pub tab_label_max_width: usize,
    /// Display 1-based tab indexes before labels.
    pub show_tab_index: bool,
    /// Which context set to render as tabs.
    pub tab_scope: StatusTabScope,
    /// How tab entries are ordered when rendered.
    pub tab_order: StatusTabOrder,
    /// Display the active session name in the status bar.
    pub show_session_name: bool,
    /// Display the current context label in the status bar.
    pub show_context_name: bool,
    /// Display the current interaction mode (Normal, Scroll, Help, etc.).
    pub show_mode: bool,
    /// Display the current attach role (write/read-only).
    pub show_role: bool,
    /// Display follow target details when following another client.
    pub show_follow: bool,
    /// Display runtime hints on the right side.
    pub show_hint: bool,
    /// Hint visibility policy.
    pub hint_policy: StatusHintPolicy,
}

impl Default for StatusBarConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            preset: StatusBarPreset::TabRail,
            layout: StatusBarLayoutConfig::default(),
            style: StatusBarStyleConfig::default(),
            colors: StatusBarColorConfig::default(),
            max_tabs: 12,
            tab_label_max_width: 20,
            show_tab_index: true,
            tab_scope: StatusTabScope::AllContexts,
            tab_order: StatusTabOrder::Stable,
            show_session_name: false,
            show_context_name: false,
            show_mode: true,
            show_role: true,
            show_follow: true,
            show_hint: true,
            hint_policy: StatusHintPolicy::ScrollOnly,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc, Default)]
#[serde(default)]
pub struct StatusBarColorConfig {
    /// Bar background color (hex `#RRGGBB`).
    pub bar_bg: Option<String>,
    /// Bar foreground color (hex `#RRGGBB`).
    pub bar_fg: Option<String>,
    /// Active tab background color (hex `#RRGGBB`).
    pub tab_active_bg: Option<String>,
    /// Active tab foreground color (hex `#RRGGBB`).
    pub tab_active_fg: Option<String>,
    /// Inactive tab background color (hex `#RRGGBB`).
    pub tab_inactive_bg: Option<String>,
    /// Inactive tab foreground color (hex `#RRGGBB`).
    pub tab_inactive_fg: Option<String>,
    /// Right-side module background color (hex `#RRGGBB`).
    pub module_bg: Option<String>,
    /// Right-side module foreground color (hex `#RRGGBB`).
    pub module_fg: Option<String>,
    /// Overflow marker background color (hex `#RRGGBB`).
    pub overflow_bg: Option<String>,
    /// Overflow marker foreground color (hex `#RRGGBB`).
    pub overflow_fg: Option<String>,
}

impl StatusBarColorConfig {
    #[must_use]
    pub const fn config_doc_values() -> &'static [&'static str] {
        &[]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[serde(default)]
pub struct StatusBarLayoutConfig {
    /// Spacing density for tabs and right modules.
    pub density: StatusDensity,
    /// Left padding before first tab.
    pub left_padding: usize,
    /// Right padding after final segment.
    pub right_padding: usize,
    /// Number of spaces between tabs.
    pub tab_gap: usize,
    /// Number of spaces between right-side modules.
    pub module_gap: usize,
    /// Overflow indicator style when tabs are hidden.
    pub overflow_style: StatusOverflowStyle,
    /// Behavior used to keep the active tab visible.
    pub align_active: StatusAlignActive,
}

impl StatusBarLayoutConfig {
    #[must_use]
    pub const fn config_doc_values() -> &'static [&'static str] {
        &[]
    }
}

impl Default for StatusBarLayoutConfig {
    fn default() -> Self {
        Self {
            density: StatusDensity::Cozy,
            left_padding: 1,
            right_padding: 1,
            tab_gap: 1,
            module_gap: 1,
            overflow_style: StatusOverflowStyle::Arrows,
            align_active: StatusAlignActive::KeepVisible,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ConfigDoc)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct StatusBarStyleConfig {
    /// Separator character set for tabs and modules.
    pub separator_set: StatusSeparatorSet,
    /// Prefer Unicode separators when available.
    pub prefer_unicode: bool,
    /// Force ASCII separators even when Unicode is enabled.
    pub force_ascii: bool,
    /// Dim inactive tabs for stronger active emphasis.
    pub dim_inactive: bool,
    /// Bold active tabs for stronger active emphasis.
    pub bold_active: bool,
    /// Underline active tabs.
    pub underline_active: bool,
}

impl StatusBarStyleConfig {
    #[must_use]
    pub const fn config_doc_values() -> &'static [&'static str] {
        &[]
    }
}

impl Default for StatusBarStyleConfig {
    fn default() -> Self {
        Self {
            separator_set: StatusSeparatorSet::AngledSegments,
            prefer_unicode: true,
            force_ascii: false,
            dim_inactive: true,
            bold_active: true,
            underline_active: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum StatusBarPreset {
    #[default]
    TabRail,
    Minimal,
    Classic,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum StatusDensity {
    Compact,
    #[default]
    Cozy,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum StatusOverflowStyle {
    Count,
    #[default]
    Arrows,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum StatusAlignActive {
    #[default]
    KeepVisible,
    FocusBias,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum StatusSeparatorSet {
    #[default]
    AngledSegments,
    Plain,
    Ascii,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum StatusTabScope {
    #[default]
    AllContexts,
    SessionContexts,
    Mru,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum StatusTabOrder {
    #[default]
    Stable,
    Mru,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq, ConfigDocEnum)]
#[serde(rename_all = "snake_case")]
pub enum StatusHintPolicy {
    #[default]
    Always,
    ScrollOnly,
    Never,
}

/// Status bar position
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, ConfigDocEnum)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StatusPosition {
    Top,
    #[default]
    Bottom,
    Off,
}

/// Border style for panes
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, ConfigDocEnum)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BorderStyle {
    #[default]
    Single,
    Double,
    Rounded,
    Thick,
    None,
}

/// Bell action behavior
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, ConfigDocEnum)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BellAction {
    None,
    #[default]
    Any,
    Current,
    Other,
}

impl BmuxConfig {
    const fn is_env_var_start(ch: char) -> bool {
        ch == '_' || ch.is_ascii_alphabetic()
    }

    const fn is_env_var_continue(ch: char) -> bool {
        ch == '_' || ch.is_ascii_alphanumeric()
    }

    fn is_valid_env_var_name(name: &str) -> bool {
        let mut chars = name.chars();
        match chars.next() {
            Some(first) if Self::is_env_var_start(first) => chars.all(Self::is_env_var_continue),
            _ => false,
        }
    }

    fn expand_home_prefix(input: &str) -> (String, bool) {
        let Some(rest) = input.strip_prefix('~') else {
            return (input.to_string(), false);
        };
        if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\')) {
            return (input.to_string(), false);
        }
        let Some(home) = dirs::home_dir() else {
            return (input.to_string(), true);
        };
        let mut expanded = home.to_string_lossy().into_owned();
        expanded.push_str(rest);
        (expanded, false)
    }

    fn expand_env_tokens(input: &str) -> (String, Vec<String>) {
        let chars: Vec<char> = input.chars().collect();
        let mut output = String::with_capacity(input.len());
        let mut unresolved = Vec::new();
        let mut index = 0usize;

        while let Some(ch) = chars.get(index).copied() {
            if ch != '$' {
                output.push(ch);
                index += 1;
                continue;
            }

            let Some(next) = chars.get(index + 1).copied() else {
                output.push('$');
                index += 1;
                continue;
            };

            if next == '$' {
                output.push('$');
                index += 2;
                continue;
            }

            if next == '{' {
                let mut end = index + 2;
                while end < chars.len() && chars[end] != '}' {
                    end += 1;
                }
                if end >= chars.len() {
                    output.push('$');
                    index += 1;
                    continue;
                }

                let name: String = chars[(index + 2)..end].iter().collect();
                if Self::is_valid_env_var_name(&name) {
                    if let Some(value) = std::env::var_os(&name) {
                        output.push_str(&value.to_string_lossy());
                    } else {
                        output.push_str("${");
                        output.push_str(&name);
                        output.push('}');
                        if !unresolved.contains(&name) {
                            unresolved.push(name);
                        }
                    }
                } else {
                    output.push_str("${");
                    output.push_str(&name);
                    output.push('}');
                }
                index = end + 1;
                continue;
            }

            if Self::is_env_var_start(next) {
                let mut end = index + 2;
                while end < chars.len() && Self::is_env_var_continue(chars[end]) {
                    end += 1;
                }

                let name: String = chars[(index + 1)..end].iter().collect();
                if let Some(value) = std::env::var_os(&name) {
                    output.push_str(&value.to_string_lossy());
                } else {
                    output.push('$');
                    output.push_str(&name);
                    if !unresolved.contains(&name) {
                        unresolved.push(name);
                    }
                }
                index = end;
                continue;
            }

            output.push('$');
            index += 1;
        }

        (output, unresolved)
    }

    fn interpolate_path_tokens(value: &std::path::Path, field: &str) -> (PathBuf, Vec<String>) {
        let input = value.to_string_lossy().into_owned();
        let (home_expanded, home_unresolved) = Self::expand_home_prefix(&input);
        let (expanded, unresolved_vars) = Self::expand_env_tokens(&home_expanded);

        let mut warnings = Vec::new();
        if home_unresolved {
            warnings.push(format!(
                "could not expand ~ in {field}; home directory is unavailable, keeping literal path"
            ));
        }
        for var in unresolved_vars {
            warnings.push(format!(
                "unresolved env var {var} in {field}; keeping literal token"
            ));
        }
        (PathBuf::from(expanded), warnings)
    }

    fn interpolate_optional_path_field(
        value: &mut Option<PathBuf>,
        field: &str,
        warnings: &mut Vec<String>,
    ) {
        if let Some(path) = value {
            let (expanded, field_warnings) = Self::interpolate_path_tokens(path.as_path(), field);
            *path = expanded;
            warnings.extend(field_warnings);
        }
    }

    fn interpolate_path_fields(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();

        Self::interpolate_optional_path_field(
            &mut self.recording.dir,
            "recording.dir",
            &mut warnings,
        );
        Self::interpolate_optional_path_field(
            &mut self.recording.auto_export_dir,
            "recording.auto_export_dir",
            &mut warnings,
        );

        for (name, target) in &mut self.connections.targets {
            let identity_field = format!("connections.targets.{name}.identity_file");
            Self::interpolate_optional_path_field(
                &mut target.identity_file,
                &identity_field,
                &mut warnings,
            );

            let known_hosts_field = format!("connections.targets.{name}.known_hosts_file");
            Self::interpolate_optional_path_field(
                &mut target.known_hosts_file,
                &known_hosts_field,
                &mut warnings,
            );

            let ca_field = format!("connections.targets.{name}.ca_file");
            Self::interpolate_optional_path_field(&mut target.ca_file, &ca_field, &mut warnings);
        }

        for (index, search_path) in self.plugins.search_paths.iter_mut().enumerate() {
            let field = format!("plugins.search_paths[{index}]");
            let (expanded, field_warnings) =
                Self::interpolate_path_tokens(search_path.as_path(), &field);
            *search_path = expanded;
            warnings.extend(field_warnings);
        }

        warnings
    }

    fn resolve_relative_to_config_dir(paths: &ConfigPaths, value: &std::path::Path) -> PathBuf {
        if value.is_absolute() {
            return value.to_path_buf();
        }
        let base = paths
            .config_file()
            .parent()
            .map_or_else(|| paths.config_dir.clone(), std::path::Path::to_path_buf);
        base.join(value)
    }

    fn env_path_override(name: &str) -> Option<PathBuf> {
        let raw = std::env::var_os(name)?;
        if raw.is_empty() {
            return None;
        }
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            return Some(path);
        }
        match std::env::current_dir() {
            Ok(cwd) => Some(cwd.join(path)),
            Err(_) => Some(path),
        }
    }

    #[must_use]
    pub fn attach_mouse_config(&self) -> MouseBehaviorConfig {
        let defaults = MouseBehaviorConfig::default();
        if self.behavior.mouse == defaults
            && self.general.mouse_support != GeneralConfig::default().mouse_support
        {
            let mut mapped = self.behavior.mouse.clone();
            mapped.enabled = self.general.mouse_support;
            return mapped;
        }
        self.behavior.mouse.clone()
    }

    /// Resolve the effective recordings directory.
    ///
    /// If `recording.dir` is set and relative, it is resolved relative to the
    /// directory that contains the active `bmux.toml`.
    #[must_use]
    pub fn recordings_dir(&self, paths: &ConfigPaths) -> PathBuf {
        if let Some(override_path) = Self::env_path_override(RECORDINGS_DIR_OVERRIDE_ENV) {
            return override_path;
        }
        self.recording.dir.as_deref().map_or_else(
            || paths.recordings_dir(),
            |dir| Self::resolve_relative_to_config_dir(paths, dir),
        )
    }

    /// Resolve `recording.auto_export_dir` if configured.
    #[must_use]
    pub fn recording_auto_export_dir(&self, paths: &ConfigPaths) -> Option<PathBuf> {
        self.recording
            .auto_export_dir
            .as_deref()
            .map(|path| Self::resolve_relative_to_config_dir(paths, path))
    }

    /// Load configuration from default location
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load() -> Result<Self> {
        let paths = ConfigPaths::default();
        Self::load_from_path(&paths.config_file())
    }

    /// Load configuration and include composition resolution metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load_with_resolution() -> Result<(Self, CompositionResolution)> {
        let paths = ConfigPaths::default();
        Self::load_from_path_with_resolution(&paths.config_file(), None)
    }

    /// Load configuration from default location using explicit layered overrides.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load_with_overrides(overrides: &ConfigLoadOverrides) -> Result<Self> {
        let paths = ConfigPaths::default();
        Self::load_from_path_with_overrides(&paths.config_file(), overrides)
    }

    /// Load configuration from explicitly-provided paths with layered overrides.
    ///
    /// Prefer this entry point from slot-aware callers so that the slot's
    /// `config_file()` is used as the primary layer.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load_with_paths_and_overrides(
        paths: &ConfigPaths,
        overrides: &ConfigLoadOverrides,
    ) -> Result<Self> {
        Self::load_from_path_with_overrides(&paths.config_file(), overrides)
    }

    /// Load configuration and include composition metadata using explicit overrides.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load_with_resolution_and_overrides(
        overrides: &ConfigLoadOverrides,
    ) -> Result<(Self, CompositionResolution)> {
        let paths = ConfigPaths::default();
        Self::load_from_path_with_resolution_and_overrides(&paths.config_file(), None, overrides)
    }

    /// Load configuration while forcing a specific active composition profile.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load_with_forced_profile(profile_id: &str) -> Result<(Self, CompositionResolution)> {
        let paths = ConfigPaths::default();
        Self::load_from_path_with_resolution(&paths.config_file(), Some(profile_id))
    }

    /// Load configuration and include layer-by-layer composition explain data.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load_with_explain(profile_id: Option<&str>) -> Result<(Self, CompositionExplain)> {
        let paths = ConfigPaths::default();
        Self::load_from_path_with_explain(&paths.config_file(), profile_id)
    }

    /// Load configuration and explain data using explicit overrides.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load_with_explain_and_overrides(
        profile_id: Option<&str>,
        overrides: &ConfigLoadOverrides,
    ) -> Result<(Self, CompositionExplain)> {
        let paths = ConfigPaths::default();
        Self::load_from_path_with_explain_and_overrides(&paths.config_file(), profile_id, overrides)
    }

    /// Load configuration from a specific path
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load_from_path(path: &std::path::Path) -> Result<Self> {
        let (config, _resolution) = Self::load_from_path_with_resolution(path, None)?;
        Ok(config)
    }

    /// Load configuration from a specific path using explicit layered overrides.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load_from_path_with_overrides(
        path: &std::path::Path,
        overrides: &ConfigLoadOverrides,
    ) -> Result<Self> {
        let (config, _resolution) =
            Self::load_from_path_with_resolution_and_overrides(path, None, overrides)?;
        Ok(config)
    }

    /// Load configuration from a specific path and return composition metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load_from_path_with_resolution(
        path: &std::path::Path,
        forced_profile: Option<&str>,
    ) -> Result<(Self, CompositionResolution)> {
        Self::load_from_path_with_resolution_and_overrides(
            path,
            forced_profile,
            &ConfigLoadOverrides::default(),
        )
    }

    /// Load configuration from a specific path and return composition metadata with overrides.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load_from_path_with_resolution_and_overrides(
        path: &std::path::Path,
        forced_profile: Option<&str>,
        overrides: &ConfigLoadOverrides,
    ) -> Result<(Self, CompositionResolution)> {
        let Some(mut raw_value) = merged_raw_config_value_with_overrides(path, Some(overrides))?
        else {
            return Ok((
                Self::default(),
                CompositionResolution {
                    selected_profile: None,
                    selected_profile_source: None,
                    matched_auto_select_index: None,
                    layer_order: vec!["defaults".to_string(), "config".to_string()],
                    available_profiles: built_in_composition_profiles().keys().cloned().collect(),
                },
            ));
        };
        apply_forced_profile(&mut raw_value, forced_profile)?;
        let (resolved_value, resolution) = resolve_composed_config_value(&raw_value)?;
        let mut config: Self = resolved_value
            .try_into()
            .map_err(|e| ConfigError::ParseError {
                error: format!("failed to deserialize resolved config: {e}"),
            })?;

        let interpolation_warnings = config.interpolate_path_fields();
        for warning in interpolation_warnings {
            eprintln!("bmux warning: {warning}");
        }

        let repaired_fields = config.sanitize_invalid_values();
        if !repaired_fields.is_empty() {
            for warning in &repaired_fields {
                eprintln!("bmux warning: repaired invalid config value {warning}");
            }
        }

        config.validate()?;
        Ok((config, resolution))
    }

    /// Load configuration from a specific path with layer-by-layer explain data.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load_from_path_with_explain(
        path: &std::path::Path,
        forced_profile: Option<&str>,
    ) -> Result<(Self, CompositionExplain)> {
        Self::load_from_path_with_explain_and_overrides(
            path,
            forced_profile,
            &ConfigLoadOverrides::default(),
        )
    }

    /// Load configuration from a specific path with layer-by-layer explain data and overrides.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be read or parsed.
    pub fn load_from_path_with_explain_and_overrides(
        path: &std::path::Path,
        forced_profile: Option<&str>,
        overrides: &ConfigLoadOverrides,
    ) -> Result<(Self, CompositionExplain)> {
        let Some(mut raw_value) = merged_raw_config_value_with_overrides(path, Some(overrides))?
        else {
            let resolution = CompositionResolution {
                selected_profile: None,
                selected_profile_source: None,
                matched_auto_select_index: None,
                layer_order: vec!["defaults".to_string(), "config".to_string()],
                available_profiles: built_in_composition_profiles().keys().cloned().collect(),
            };
            return Ok((
                Self::default(),
                CompositionExplain {
                    resolution,
                    applied_layers: Vec::new(),
                },
            ));
        };
        apply_forced_profile(&mut raw_value, forced_profile)?;
        let (resolved_value, resolution, applied_layers) =
            resolve_composed_config_value_with_explain(&raw_value)?;
        let mut config: Self = resolved_value
            .try_into()
            .map_err(|e| ConfigError::ParseError {
                error: format!("failed to deserialize resolved config: {e}"),
            })?;

        let interpolation_warnings = config.interpolate_path_fields();
        for warning in interpolation_warnings {
            eprintln!("bmux warning: {warning}");
        }

        let repaired_fields = config.sanitize_invalid_values();
        if !repaired_fields.is_empty() {
            for warning in &repaired_fields {
                eprintln!("bmux warning: repaired invalid config value {warning}");
            }
        }

        config.validate()?;
        Ok((
            config,
            CompositionExplain {
                resolution,
                applied_layers,
            },
        ))
    }

    /// Save configuration to default location
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be written.
    pub fn save(&self) -> Result<()> {
        let paths = ConfigPaths::default();
        self.save_to_path(&paths.config_file())
    }

    /// Save configuration to a specific path
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration file cannot be written.
    pub fn save_to_path(&self, path: &std::path::Path) -> Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let contents = toml::to_string_pretty(self).map_err(|e| ConfigError::ParseError {
            error: e.to_string(),
        })?;

        std::fs::write(path, contents)?;
        Ok(())
    }

    /// Validate the configuration
    ///
    /// # Errors
    ///
    /// Returns an error if any configuration values are invalid.
    pub fn validate(&self) -> Result<()> {
        // Validate scrollback limit
        if self.general.scrollback_limit == 0 {
            return Err(ConfigError::InvalidValue {
                field: "general.scrollback_limit".to_string(),
                value: "0".to_string(),
            });
        }

        // Validate server timeout
        if self.general.server_timeout == 0 {
            return Err(ConfigError::InvalidValue {
                field: "general.server_timeout".to_string(),
                value: "0".to_string(),
            });
        }

        if let Err(error) = self.keybindings.resolve_timeout() {
            return Err(ConfigError::InvalidValue {
                field: "keybindings".to_string(),
                value: error,
            });
        }

        if let Err(error) = self.keybindings.validate_modes() {
            return Err(ConfigError::InvalidValue {
                field: "keybindings.modes".to_string(),
                value: error,
            });
        }

        if self.behavior.mouse.hover_delay_ms == 0 {
            return Err(ConfigError::InvalidValue {
                field: "behavior.mouse.hover_delay_ms".to_string(),
                value: "0".to_string(),
            });
        }

        if self.behavior.mouse.scroll_lines_per_tick == 0 {
            return Err(ConfigError::InvalidValue {
                field: "behavior.mouse.scroll_lines_per_tick".to_string(),
                value: "0".to_string(),
            });
        }

        if self.recording.export.cursor_blink_period_ms == 0 {
            return Err(ConfigError::InvalidValue {
                field: "recording.export.cursor_blink_period_ms".to_string(),
                value: "0".to_string(),
            });
        }
        if self.recording.export.cursor_bar_width_pct == 0
            || self.recording.export.cursor_bar_width_pct > 100
        {
            return Err(ConfigError::InvalidValue {
                field: "recording.export.cursor_bar_width_pct".to_string(),
                value: self.recording.export.cursor_bar_width_pct.to_string(),
            });
        }
        if self.recording.export.cursor_underline_height_pct == 0
            || self.recording.export.cursor_underline_height_pct > 100
        {
            return Err(ConfigError::InvalidValue {
                field: "recording.export.cursor_underline_height_pct".to_string(),
                value: self
                    .recording
                    .export
                    .cursor_underline_height_pct
                    .to_string(),
            });
        }

        if self.performance.window_ms == 0 {
            return Err(ConfigError::InvalidValue {
                field: "performance.window_ms".to_string(),
                value: "0".to_string(),
            });
        }

        if self.performance.max_events_per_sec == 0 {
            return Err(ConfigError::InvalidValue {
                field: "performance.max_events_per_sec".to_string(),
                value: "0".to_string(),
            });
        }

        if self.performance.max_payload_bytes_per_sec == 0 {
            return Err(ConfigError::InvalidValue {
                field: "performance.max_payload_bytes_per_sec".to_string(),
                value: "0".to_string(),
            });
        }

        if self.status_bar.max_tabs == 0 {
            return Err(ConfigError::InvalidValue {
                field: "status_bar.max_tabs".to_string(),
                value: "0".to_string(),
            });
        }

        if self.status_bar.tab_label_max_width == 0 {
            return Err(ConfigError::InvalidValue {
                field: "status_bar.tab_label_max_width".to_string(),
                value: "0".to_string(),
            });
        }

        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn sanitize_invalid_values(&mut self) -> Vec<String> {
        let general_defaults = GeneralConfig::default();
        let keybind_defaults = KeyBindingConfig::default();
        let behavior_defaults = BehaviorConfig::default();
        let recording_defaults = RecordingConfig::default();
        let performance_defaults = PerformanceConfig::default();
        let mut repaired_fields = Vec::new();

        if self.general.scrollback_limit == 0 {
            self.general.scrollback_limit = general_defaults.scrollback_limit;
            repaired_fields.push(format!(
                "general.scrollback_limit=0 -> {}",
                general_defaults.scrollback_limit
            ));
        }

        if self.general.server_timeout == 0 {
            self.general.server_timeout = general_defaults.server_timeout;
            repaired_fields.push(format!(
                "general.server_timeout=0 -> {}",
                general_defaults.server_timeout
            ));
        }

        if let Err(error) = self.keybindings.resolve_timeout() {
            self.keybindings.timeout_ms = keybind_defaults.timeout_ms;
            self.keybindings
                .timeout_profile
                .clone_from(&keybind_defaults.timeout_profile);
            self.keybindings.timeout_profiles = keybind_defaults.timeout_profiles;
            repaired_fields.push(format!(
                "keybindings timeout settings -> indefinite ({error})"
            ));
        }

        if self.behavior.protocol_trace_capacity == 0 {
            self.behavior.protocol_trace_capacity = behavior_defaults.protocol_trace_capacity;
            repaired_fields.push(format!(
                "behavior.protocol_trace_capacity=0 -> {}",
                behavior_defaults.protocol_trace_capacity
            ));
        }

        if self.behavior.mouse == MouseBehaviorConfig::default()
            && self.general.mouse_support != general_defaults.mouse_support
        {
            self.behavior.mouse.enabled = self.general.mouse_support;
            repaired_fields.push(format!(
                "general.mouse_support={} -> behavior.mouse.enabled={}",
                self.general.mouse_support, self.general.mouse_support
            ));
        }

        if self.behavior.mouse.hover_delay_ms == 0 {
            self.behavior.mouse.hover_delay_ms = MouseBehaviorConfig::default().hover_delay_ms;
            repaired_fields.push(format!(
                "behavior.mouse.hover_delay_ms=0 -> {}",
                self.behavior.mouse.hover_delay_ms
            ));
        }

        if self.behavior.mouse.scroll_lines_per_tick == 0 {
            self.behavior.mouse.scroll_lines_per_tick =
                MouseBehaviorConfig::default().scroll_lines_per_tick;
            repaired_fields.push(format!(
                "behavior.mouse.scroll_lines_per_tick=0 -> {}",
                self.behavior.mouse.scroll_lines_per_tick
            ));
        }

        if self.recording.segment_mb == 0 {
            self.recording.segment_mb = recording_defaults.segment_mb;
            repaired_fields.push(format!(
                "recording.segment_mb=0 -> {}",
                recording_defaults.segment_mb
            ));
        }

        if self.recording.export.cursor_blink_period_ms == 0 {
            self.recording.export.cursor_blink_period_ms =
                recording_defaults.export.cursor_blink_period_ms;
            repaired_fields.push(format!(
                "recording.export.cursor_blink_period_ms=0 -> {}",
                self.recording.export.cursor_blink_period_ms
            ));
        }
        if self.recording.export.cursor_bar_width_pct == 0
            || self.recording.export.cursor_bar_width_pct > 100
        {
            self.recording.export.cursor_bar_width_pct =
                recording_defaults.export.cursor_bar_width_pct;
            repaired_fields.push(format!(
                "recording.export.cursor_bar_width_pct out of range -> {}",
                self.recording.export.cursor_bar_width_pct
            ));
        }
        if self.recording.export.cursor_underline_height_pct == 0
            || self.recording.export.cursor_underline_height_pct > 100
        {
            self.recording.export.cursor_underline_height_pct =
                recording_defaults.export.cursor_underline_height_pct;
            repaired_fields.push(format!(
                "recording.export.cursor_underline_height_pct out of range -> {}",
                self.recording.export.cursor_underline_height_pct
            ));
        }

        if self.performance.window_ms == 0 {
            self.performance.window_ms = performance_defaults.window_ms;
            repaired_fields.push(format!(
                "performance.window_ms=0 -> {}",
                self.performance.window_ms
            ));
        }

        if self.performance.max_events_per_sec == 0 {
            self.performance.max_events_per_sec = performance_defaults.max_events_per_sec;
            repaired_fields.push(format!(
                "performance.max_events_per_sec=0 -> {}",
                self.performance.max_events_per_sec
            ));
        }

        if self.performance.max_payload_bytes_per_sec == 0 {
            self.performance.max_payload_bytes_per_sec =
                performance_defaults.max_payload_bytes_per_sec;
            repaired_fields.push(format!(
                "performance.max_payload_bytes_per_sec=0 -> {}",
                self.performance.max_payload_bytes_per_sec
            ));
        }

        if self.status_bar.max_tabs == 0 {
            self.status_bar.max_tabs = StatusBarConfig::default().max_tabs;
            repaired_fields.push(format!(
                "status_bar.max_tabs=0 -> {}",
                self.status_bar.max_tabs
            ));
        }

        if self.status_bar.tab_label_max_width == 0 {
            self.status_bar.tab_label_max_width = StatusBarConfig::default().tab_label_max_width;
            repaired_fields.push(format!(
                "status_bar.tab_label_max_width=0 -> {}",
                self.status_bar.tab_label_max_width
            ));
        }

        repaired_fields
    }

    /// Merge another configuration into this one
    pub fn merge(&mut self, other: Self) {
        let Ok(mut base) = toml::Value::try_from(&*self) else {
            *self = other;
            return;
        };
        let Ok(overlay) = toml::Value::try_from(&other) else {
            *self = other;
            return;
        };
        merge_toml_value(&mut base, overlay);
        if let Ok(merged) = base.try_into() {
            *self = merged;
        } else {
            *self = other;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BMUX_CONFIG_ENV, BmuxConfig, ConfigLoadOverrides, MouseClickPropagation,
        MouseWheelPropagation, ResolvedTimeout, SandboxCleanupSource, StaleBuildAction,
        push_process_config_overrides,
    };
    use crate::ConfigPaths;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_config_path() -> std::path::PathBuf {
        let unique = format!(
            "bmux-config-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before epoch")
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).expect("failed to create temp test directory");
        dir.join("bmux.toml")
    }

    fn env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            // SAFETY: test-scoped env mutation guarded by env_lock.
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            // SAFETY: test-scoped env mutation guarded by env_lock.
            unsafe { std::env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.as_ref() {
                // SAFETY: restoring process env in test teardown.
                unsafe { std::env::set_var(self.key, previous) };
            } else {
                // SAFETY: restoring process env in test teardown.
                unsafe { std::env::remove_var(self.key) };
            }
        }
    }

    struct CwdGuard {
        previous: std::path::PathBuf,
    }

    impl CwdGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::current_dir().expect("current dir");
            std::env::set_current_dir(path).expect("set current dir");
            Self { previous }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.previous).expect("restore current dir");
        }
    }

    #[test]
    fn default_config_is_valid() {
        let config = BmuxConfig::default();
        assert!(config.validate().is_ok());
        assert_eq!(config.behavior.stale_build_action, StaleBuildAction::Ignore);
        assert!(config.plugins.enabled.is_empty());
        assert!(config.plugins.disabled.is_empty());
        assert!(config.plugins.routing.required_namespaces.is_empty());
        assert!(config.plugins.routing.required_paths.is_empty());
        assert_eq!(
            config
                .keybindings
                .resolve_timeout()
                .expect("default timeout"),
            ResolvedTimeout::Indefinite
        );
    }

    #[test]
    fn load_missing_file_returns_defaults_without_creating_file() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();

        let config = BmuxConfig::load_from_path(&path).expect("failed loading missing config");
        let defaults = BmuxConfig::default();

        assert!(!path.exists());
        assert_eq!(
            config.general.scrollback_limit,
            defaults.general.scrollback_limit
        );
        assert_eq!(
            config.general.server_timeout,
            defaults.general.server_timeout
        );
        assert_eq!(config.keybindings.prefix, defaults.keybindings.prefix);
        assert_eq!(
            config.keybindings.timeout_ms,
            defaults.keybindings.timeout_ms
        );
        assert_eq!(
            config.keybindings.timeout_profile,
            defaults.keybindings.timeout_profile
        );
        assert!(config.plugins.disabled.is_empty());

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn load_parses_plugin_disabled_list() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(
            &path,
            "[plugins]\nenabled = ['bmux.windows']\ndisabled = ['bmux.permissions']\n",
        )
        .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.plugins.enabled, vec!["bmux.windows".to_string()]);
        assert_eq!(
            config.plugins.disabled,
            vec!["bmux.permissions".to_string()]
        );

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn load_parses_plugin_routing_policy_claims() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(
            &path,
            "[plugins.routing]\nconflict_mode = 'fail_startup'\n[[plugins.routing.required_namespaces]]\nnamespace = 'logs'\nowner = 'third.party.logs'\n[[plugins.routing.required_paths]]\npath = ['playbook','run']\n",
        )
        .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.plugins.routing.required_namespaces.len(), 1);
        assert_eq!(config.plugins.routing.required_paths.len(), 1);
        assert_eq!(
            config.plugins.routing.required_namespaces[0]
                .owner
                .as_deref(),
            Some("third.party.logs")
        );
        assert_eq!(
            config.plugins.routing.required_paths[0].path,
            vec!["playbook".to_string(), "run".to_string()]
        );

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn load_parses_connection_targets() {
        let path = temp_config_path();
        std::fs::write(
            &path,
            "[connections]\ndefault_target = 'prod'\n[connections.targets.prod]\ntransport = 'ssh'\nhost = 'prod.example.com'\nport = 2222\n",
        )
        .expect("write temp config");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.connections.default_target.as_deref(), Some("prod"));
        let prod = config
            .connections
            .targets
            .get("prod")
            .expect("prod target missing");
        assert_eq!(prod.host.as_deref(), Some("prod.example.com"));
        assert_eq!(prod.port, Some(2222));
    }

    #[test]
    fn composition_active_profile_applies_full_config_patch() {
        let path = temp_config_path();
        std::fs::write(
            &path,
            r#"
[composition]
active_profile = "dev"
layer_order = ["defaults", "config", "profile:active"]

[composition.profiles.dev.patch.general]
server_timeout = 7777

[composition.profiles.dev.patch.behavior]
pane_term = "xterm-256color"
"#,
        )
        .expect("write temp config");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.general.server_timeout, 7777);
        assert_eq!(config.behavior.pane_term, "xterm-256color");
    }

    #[test]
    fn composition_multiple_parent_rightmost_wins() {
        let path = temp_config_path();
        std::fs::write(
            &path,
            r#"
[composition]
active_profile = "child"

[composition.profiles.left.patch.general]
server_timeout = 100

[composition.profiles.right.patch.general]
server_timeout = 200

[composition.profiles.child]
extends = ["left", "right"]
"#,
        )
        .expect("write temp config");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.general.server_timeout, 200);
    }

    #[test]
    fn composition_built_in_vim_profile_loads_file_backed_modes() {
        let path = temp_config_path();
        std::fs::write(
            &path,
            r#"
[composition]
active_profile = "vim"
"#,
        )
        .expect("write temp config");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(
            config
                .keybindings
                .modes
                .get("normal")
                .and_then(|mode| mode.bindings.get(":")),
            Some(&"enter_mode command".to_string())
        );
        assert!(config.keybindings.modes.contains_key("visual"));
        assert!(config.keybindings.modes.contains_key("command"));
    }

    #[test]
    fn composition_user_profile_can_extend_built_in_profiles() {
        let path = temp_config_path();
        std::fs::write(
            &path,
            r#"
[composition]
active_profile = "team"

[composition.profiles.team]
extends = ["tmux_compat", "zellij_compat"]
"#,
        )
        .expect("write temp config");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.keybindings.initial_mode, "normal");
        assert_eq!(config.keybindings.prefix, "ctrl+b");
        assert_eq!(
            config
                .keybindings
                .modes
                .get("insert")
                .and_then(|mode| mode.bindings.get("ctrl+b")),
            Some(&"enter_mode normal".to_string())
        );
        assert_eq!(
            config.keybindings.global.get("alt+n"),
            Some(&"split_focused_horizontal".to_string())
        );
    }

    #[test]
    fn composition_built_in_zellij_extends_built_in_vim_profile() {
        let path = temp_config_path();
        std::fs::write(
            &path,
            r#"
[composition]
active_profile = "zellij_compat"
"#,
        )
        .expect("write temp config");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(
            config
                .keybindings
                .modes
                .get("normal")
                .and_then(|mode| mode.bindings.get(":")),
            Some(&"enter_mode command".to_string())
        );
        assert_eq!(
            config.keybindings.global.get("alt+n"),
            Some(&"split_focused_horizontal".to_string())
        );
        assert_eq!(
            config.keybindings.global.get("alt+v"),
            Some(&"split_focused_vertical".to_string())
        );
    }

    #[test]
    fn composition_layer_order_is_configurable() {
        let path = temp_config_path();
        std::fs::write(
            &path,
            r#"
[composition]
active_profile = "dev"
layer_order = ["defaults", "config", "profile:active"]

[general]
server_timeout = 200

[composition.profiles.dev.patch.general]
server_timeout = 900
"#,
        )
        .expect("write temp config");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.general.server_timeout, 900);
    }

    #[test]
    fn composition_rejects_inheritance_cycles() {
        let path = temp_config_path();
        std::fs::write(
            &path,
            r#"
[composition]
active_profile = "a"

[composition.profiles.a]
extends = ["b"]

[composition.profiles.b]
extends = ["a"]
"#,
        )
        .expect("write temp config");

        let error = BmuxConfig::load_from_path(&path).expect_err("cycle should fail");
        match error {
            super::ConfigError::InvalidValue { field, value } => {
                assert_eq!(field, "composition.profiles");
                assert!(value.contains("cycle"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn composition_arrays_replace_instead_of_append() {
        let path = temp_config_path();
        std::fs::write(
            &path,
            r#"
[composition]
active_profile = "dev"
layer_order = ["defaults", "config", "profile:active"]

[plugins]
enabled = ["one", "two"]

[composition.profiles.dev.patch.plugins]
enabled = ["three"]
"#,
        )
        .expect("write temp config");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.plugins.enabled, vec!["three".to_string()]);
    }

    #[test]
    fn composition_auto_select_uses_first_matching_rule() {
        let path = temp_config_path();
        std::fs::write(
            &path,
            r#"
[composition]
layer_order = ["defaults", "profile:active", "config"]

[[composition.auto_select]]
profile = "first"
os = "macos"

[[composition.auto_select]]
profile = "second"
os = "macos"

[composition.profiles.first.patch.general]
server_timeout = 111

[composition.profiles.second.patch.general]
server_timeout = 222
"#,
        )
        .expect("write temp config");

        let (config, resolution) =
            BmuxConfig::load_from_path_with_resolution(&path, None).expect("failed loading config");
        assert_eq!(config.general.server_timeout, 111);
        assert_eq!(resolution.selected_profile, Some("first".to_string()));
        assert_eq!(resolution.matched_auto_select_index, Some(0));
    }

    #[test]
    fn composition_forced_profile_overrides_auto_select() {
        let path = temp_config_path();
        std::fs::write(
            &path,
            r#"
[composition]
layer_order = ["defaults", "profile:active", "config"]

[[composition.auto_select]]
profile = "first"
os = "macos"

[composition.profiles.first.patch.general]
server_timeout = 111

[composition.profiles.second.patch.general]
server_timeout = 222
"#,
        )
        .expect("write temp config");

        let (config, resolution) =
            BmuxConfig::load_from_path_with_resolution(&path, Some("second"))
                .expect("failed loading config");
        assert_eq!(config.general.server_timeout, 222);
        assert_eq!(resolution.selected_profile, Some("second".to_string()));
        assert_eq!(
            resolution.selected_profile_source,
            Some("composition.active_profile".to_string())
        );
    }

    #[test]
    fn composition_explain_reports_changed_paths_per_layer() {
        let path = temp_config_path();
        std::fs::write(
            &path,
            r#"
[composition]
active_profile = "tmux_compat"

[general]
server_timeout = 1234
"#,
        )
        .expect("write temp config");

        let (_config, explain) =
            BmuxConfig::load_from_path_with_explain(&path, None).expect("failed loading config");
        assert_eq!(
            explain.resolution.selected_profile.as_deref(),
            Some("tmux_compat")
        );
        let profile_layer = explain
            .applied_layers
            .iter()
            .find(|layer| layer.layer == "profile:active")
            .expect("profile layer should exist");
        assert!(
            profile_layer
                .changed_paths
                .iter()
                .any(|path| path == "keybindings.prefix")
        );
        let config_layer = explain
            .applied_layers
            .iter()
            .find(|layer| layer.layer == "config")
            .expect("config layer should exist");
        assert!(
            config_layer
                .changed_paths
                .iter()
                .any(|path| path == "general.server_timeout")
        );
    }

    #[test]
    fn load_expands_connection_target_path_fields() {
        let Some(home) = dirs::home_dir() else {
            return;
        };

        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(
            &path,
            "[connections.targets.prod]\ntransport = 'ssh'\nidentity_file = '$HOME/.ssh/id_ed25519'\nknown_hosts_file = '${HOME}/.ssh/known_hosts'\nca_file = '~/certs/prod-ca.pem'\n",
        )
        .expect("write temp config");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        let prod = config
            .connections
            .targets
            .get("prod")
            .expect("prod target missing");
        assert_eq!(
            prod.identity_file,
            Some(home.join(".ssh").join("id_ed25519"))
        );
        assert_eq!(
            prod.known_hosts_file,
            Some(home.join(".ssh").join("known_hosts"))
        );
        assert_eq!(prod.ca_file, Some(home.join("certs").join("prod-ca.pem")));

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn load_expands_plugin_search_paths() {
        let Some(home) = dirs::home_dir() else {
            return;
        };

        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(
            &path,
            "[plugins]\nsearch_paths = ['$HOME/.local/share/bmux/plugins', '~/bmux/plugins']\n",
        )
        .expect("write temp config");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(
            config.plugins.search_paths,
            vec![
                home.join(".local")
                    .join("share")
                    .join("bmux")
                    .join("plugins"),
                home.join("bmux").join("plugins"),
            ]
        );

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn load_repairs_zero_general_limits_without_persisting() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();

        std::fs::write(
            &path,
            "[general]\nscrollback_limit = 0\nserver_timeout = 0\n",
        )
        .expect("failed writing invalid config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.general.scrollback_limit, 10_000);
        assert_eq!(config.general.server_timeout, 5_000);

        let persisted = std::fs::read_to_string(&path).expect("failed reading config file");
        assert!(persisted.contains("scrollback_limit = 0"));
        assert!(persisted.contains("server_timeout = 0"));

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn load_repairs_invalid_fields_and_keeps_valid_behavior_settings_without_persisting() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();

        std::fs::write(
            &path,
            r#"[general]
scrollback_limit = 0
server_timeout = 5000

[behavior]
pane_term = "xterm-256color"
protocol_trace_enabled = true
protocol_trace_capacity = 0

[keybindings]
prefix = ""
timeout_profile = "warp"

[keybindings.timeout_profiles]
fast = 10
"#,
        )
        .expect("failed writing invalid config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.general.scrollback_limit, 10_000);
        assert_eq!(config.general.server_timeout, 5_000);
        assert_eq!(config.keybindings.prefix, "");
        assert_eq!(
            config
                .keybindings
                .resolve_timeout()
                .expect("resolved timeout"),
            ResolvedTimeout::Indefinite
        );
        assert_eq!(config.behavior.pane_term, "xterm-256color");
        assert!(config.behavior.protocol_trace_enabled);
        assert_eq!(config.behavior.protocol_trace_capacity, 200);

        let persisted = std::fs::read_to_string(&path).expect("failed reading config file");
        assert!(persisted.contains("scrollback_limit = 0"));
        assert!(persisted.contains("prefix = \"\""));
        assert!(persisted.contains("timeout_profile = \"warp\""));
        assert!(persisted.contains("fast = 10"));
        assert!(persisted.contains("protocol_trace_capacity = 0"));
        assert!(persisted.contains("pane_term = \"xterm-256color\""));

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn timeout_profile_resolves_with_override() {
        let mut config = BmuxConfig::default();
        config.keybindings.timeout_profile = Some("traditional".to_string());
        config
            .keybindings
            .timeout_profiles
            .insert("traditional".to_string(), 450);

        assert_eq!(
            config
                .keybindings
                .resolve_timeout()
                .expect("resolved timeout"),
            ResolvedTimeout::Profile {
                name: "traditional".to_string(),
                ms: 450,
            }
        );
    }

    #[test]
    fn exact_timeout_takes_precedence_over_profile() {
        let mut config = BmuxConfig::default();
        config.keybindings.timeout_ms = Some(275);
        config.keybindings.timeout_profile = Some("slow".to_string());

        assert_eq!(
            config
                .keybindings
                .resolve_timeout()
                .expect("resolved timeout"),
            ResolvedTimeout::Exact(275)
        );
    }

    #[test]
    fn invalid_profile_repairs_to_indefinite_without_persisting() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(
            &path,
            r#"[keybindings]
timeout_profile = "missing"
"#,
        )
        .expect("failed writing invalid config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(
            config
                .keybindings
                .resolve_timeout()
                .expect("resolved timeout"),
            ResolvedTimeout::Indefinite
        );
        assert_eq!(config.keybindings.timeout_profile, None);

        let persisted = std::fs::read_to_string(&path).expect("failed reading config file");
        assert!(persisted.contains("timeout_profile = \"missing\""));

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn load_parses_warn_stale_build_action() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(&path, "[behavior]\nstale_build_action = \"warn\"\n")
            .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.behavior.stale_build_action, StaleBuildAction::Warn);

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn load_parses_ignore_stale_build_action() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(&path, "[behavior]\nstale_build_action = \"ignore\"\n")
            .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.behavior.stale_build_action, StaleBuildAction::Ignore);

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn attach_mouse_config_maps_legacy_general_mouse_support_when_mouse_block_uses_defaults() {
        let mut config = BmuxConfig::default();
        config.general.mouse_support = false;

        let mouse = config.attach_mouse_config();
        assert!(!mouse.enabled);
    }

    #[test]
    fn load_repairs_invalid_mouse_values_without_persisting() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(
            &path,
            "[behavior.mouse]\nhover_delay_ms = 0\nscroll_lines_per_tick = 0\n",
        )
        .expect("failed writing invalid config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.behavior.mouse.hover_delay_ms, 175);
        assert_eq!(config.behavior.mouse.scroll_lines_per_tick, 3);

        let persisted = std::fs::read_to_string(&path).expect("failed reading config file");
        assert!(persisted.contains("hover_delay_ms = 0"));
        assert!(persisted.contains("scroll_lines_per_tick = 0"));

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn mouse_defaults_prioritize_tui_forwarding() {
        let mouse = BmuxConfig::default().behavior.mouse;
        assert_eq!(
            mouse.click_propagation,
            MouseClickPropagation::FocusAndForward
        );
        assert_eq!(mouse.wheel_propagation, MouseWheelPropagation::ForwardOnly);
        assert!(mouse.resize_borders);
    }

    #[test]
    fn load_parses_mouse_resize_borders_toggle() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(&path, "[behavior.mouse]\nresize_borders = false\n")
            .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert!(!config.behavior.mouse.resize_borders);

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn load_parses_mouse_propagation_modes() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(
            &path,
            "[behavior.mouse]\nclick_propagation = \"forward_only\"\nwheel_propagation = \"forward_and_scrollback\"\n",
        )
        .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(
            config.behavior.mouse.click_propagation,
            MouseClickPropagation::ForwardOnly
        );
        assert_eq!(
            config.behavior.mouse.wheel_propagation,
            MouseWheelPropagation::ForwardAndScrollback
        );

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn effective_mouse_propagation_honors_legacy_flags() {
        let mut config = BmuxConfig::default();
        config.behavior.mouse.focus_on_click = false;
        config.behavior.mouse.wheel_propagation = MouseWheelPropagation::ForwardAndScrollback;
        config.behavior.mouse.scroll_scrollback = false;

        assert_eq!(
            config.behavior.mouse.effective_click_propagation(),
            MouseClickPropagation::ForwardOnly
        );
        assert_eq!(
            config.behavior.mouse.effective_wheel_propagation(),
            MouseWheelPropagation::ForwardOnly
        );
    }

    #[test]
    fn recordings_dir_uses_default_when_unset() {
        let paths = ConfigPaths::new(
            std::path::PathBuf::from("/config"),
            std::path::PathBuf::from("/runtime"),
            std::path::PathBuf::from("/data"),
            std::path::PathBuf::from("/state"),
        );
        let config = BmuxConfig::default();

        assert_eq!(config.recordings_dir(&paths), paths.recordings_dir());
    }

    #[test]
    fn recordings_dir_uses_absolute_override() {
        let paths = ConfigPaths::new(
            std::path::PathBuf::from("/config"),
            std::path::PathBuf::from("/runtime"),
            std::path::PathBuf::from("/data"),
            std::path::PathBuf::from("/state"),
        );
        let mut config = BmuxConfig::default();
        config.recording.dir = Some(std::path::PathBuf::from("/custom/recordings"));

        assert_eq!(
            config.recordings_dir(&paths),
            std::path::PathBuf::from("/custom/recordings")
        );
    }

    #[test]
    fn recordings_dir_resolves_relative_to_config_file_directory() {
        let paths = ConfigPaths::new(
            std::path::PathBuf::from("/cfg-root"),
            std::path::PathBuf::from("/runtime"),
            std::path::PathBuf::from("/data"),
            std::path::PathBuf::from("/state"),
        );
        let mut config = BmuxConfig::default();
        config.recording.dir = Some(std::path::PathBuf::from("recordings/custom"));

        assert_eq!(
            config.recordings_dir(&paths),
            std::path::PathBuf::from("/cfg-root/recordings/custom")
        );
    }

    #[test]
    fn load_parses_recording_dir_override() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(&path, "[recording]\ndir = 'recordings/custom'\n")
            .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(
            config.recording.dir,
            Some(std::path::PathBuf::from("recordings/custom"))
        );

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn load_expands_recording_path_fields() {
        let Some(home) = dirs::home_dir() else {
            return;
        };

        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(
            &path,
            "[recording]\ndir = '~/bmux-recordings'\nauto_export = true\nauto_export_dir = '${HOME}/bmux-gifs'\n",
        )
        .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(config.recording.dir, Some(home.join("bmux-recordings")));
        assert_eq!(
            config.recording.auto_export_dir,
            Some(home.join("bmux-gifs"))
        );

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn interpolate_path_tokens_keeps_unresolved_vars_literal_and_reports_warning() {
        let mut suffix = 0usize;
        let missing = loop {
            let candidate = format!("BMUX_CONFIG_TEST_MISSING_{}_{}", std::process::id(), suffix);
            if std::env::var_os(&candidate).is_none() {
                break candidate;
            }
            suffix += 1;
        };
        let raw = format!("${missing}/dir");
        let (expanded, warnings) =
            BmuxConfig::interpolate_path_tokens(std::path::Path::new(&raw), "recording.dir");

        assert_eq!(expanded, std::path::PathBuf::from(raw));
        assert!(warnings.iter().any(|warning| warning.contains(&missing)));
    }

    #[test]
    fn interpolate_path_tokens_supports_escaped_dollar() {
        let (expanded, warnings) =
            BmuxConfig::interpolate_path_tokens(std::path::Path::new("$$HOME/cache"), "field");

        assert_eq!(expanded, std::path::PathBuf::from("$HOME/cache"));
        assert!(warnings.is_empty());
    }

    #[test]
    fn recording_auto_export_defaults_disabled_without_custom_dir() {
        let config = BmuxConfig::default();
        assert!(!config.recording.auto_export);
        assert!(config.recording.auto_export_dir.is_none());
    }

    #[test]
    fn recording_auto_export_dir_uses_absolute_override() {
        let paths = ConfigPaths::new(
            std::path::PathBuf::from("/config"),
            std::path::PathBuf::from("/runtime"),
            std::path::PathBuf::from("/data"),
            std::path::PathBuf::from("/state"),
        );
        let mut config = BmuxConfig::default();
        config.recording.auto_export_dir = Some(std::path::PathBuf::from("/exports/gif"));

        assert_eq!(
            config.recording_auto_export_dir(&paths),
            Some(std::path::PathBuf::from("/exports/gif"))
        );
    }

    #[test]
    fn recording_auto_export_dir_resolves_relative_to_config_file_directory() {
        let paths = ConfigPaths::new(
            std::path::PathBuf::from("/cfg-root"),
            std::path::PathBuf::from("/runtime"),
            std::path::PathBuf::from("/data"),
            std::path::PathBuf::from("/state"),
        );
        let mut config = BmuxConfig::default();
        config.recording.auto_export_dir = Some(std::path::PathBuf::from("exports/gif"));

        assert_eq!(
            config.recording_auto_export_dir(&paths),
            Some(std::path::PathBuf::from("/cfg-root/exports/gif"))
        );
    }

    #[test]
    fn load_parses_recording_auto_export_settings() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(
            &path,
            "[recording]\nauto_export = true\nauto_export_dir = 'exports/gif'\n",
        )
        .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert!(config.recording.auto_export);
        assert_eq!(
            config.recording.auto_export_dir,
            Some(std::path::PathBuf::from("exports/gif"))
        );

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn sandbox_cleanup_defaults_are_stable() {
        let config = BmuxConfig::default();
        assert!(!config.sandbox.cleanup.failed_only);
        assert_eq!(config.sandbox.cleanup.older_than_secs, 300);
        assert_eq!(config.sandbox.cleanup.source, SandboxCleanupSource::All);
    }

    #[test]
    fn load_parses_sandbox_cleanup_defaults() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(
            &path,
            "[sandbox.cleanup]\nfailed_only = true\nolder_than_secs = 42\nsource = 'playbook'\n",
        )
        .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert!(config.sandbox.cleanup.failed_only);
        assert_eq!(config.sandbox.cleanup.older_than_secs, 42);
        assert_eq!(
            config.sandbox.cleanup.source,
            SandboxCleanupSource::Playbook
        );

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn recording_export_defaults_include_cursor_settings() {
        let config = BmuxConfig::default();
        assert_eq!(
            config.recording.export.cursor,
            crate::RecordingExportCursorMode::Auto
        );
        assert_eq!(
            config.recording.export.cursor_shape,
            crate::RecordingExportCursorShape::Auto
        );
        assert_eq!(
            config.recording.export.cursor_blink,
            crate::RecordingExportCursorBlinkMode::Auto
        );
        assert_eq!(config.recording.export.cursor_blink_period_ms, 500);
        assert_eq!(config.recording.export.cursor_color, "auto");
        assert_eq!(
            config.recording.export.cursor_profile,
            crate::RecordingExportCursorProfile::Auto
        );
        assert_eq!(config.recording.export.cursor_solid_after_activity_ms, None);
        assert_eq!(config.recording.export.cursor_solid_after_input_ms, None);
        assert_eq!(config.recording.export.cursor_solid_after_output_ms, None);
        assert_eq!(config.recording.export.cursor_solid_after_cursor_ms, None);
        assert_eq!(
            config.recording.export.cursor_paint_mode,
            crate::RecordingExportCursorPaintMode::Auto
        );
        assert_eq!(
            config.recording.export.cursor_text_mode,
            crate::RecordingExportCursorTextMode::Auto
        );
        assert_eq!(config.recording.export.cursor_bar_width_pct, 16);
        assert_eq!(config.recording.export.cursor_underline_height_pct, 12);
        assert_eq!(
            config.recording.export.palette_source,
            crate::RecordingExportPaletteSource::Auto
        );
        assert_eq!(config.recording.export.palette_foreground, None);
        assert_eq!(config.recording.export.palette_background, None);
        assert!(config.recording.export.palette_colors.is_empty());
    }

    #[test]
    fn load_parses_recording_export_cursor_defaults() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(
            &path,
            "[recording.export]\ncursor = 'on'\ncursor_shape = 'underline'\ncursor_blink = 'off'\ncursor_blink_period_ms = 650\ncursor_color = '#44aaee'\ncursor_profile = 'ghostty'\ncursor_solid_after_activity_ms = 900\ncursor_solid_after_input_ms = 910\ncursor_solid_after_output_ms = 920\ncursor_solid_after_cursor_ms = 930\ncursor_paint_mode = 'fill'\ncursor_text_mode = 'swap_fg_bg'\ncursor_bar_width_pct = 14\ncursor_underline_height_pct = 11\npalette_source = 'recording'\npalette_foreground = '#d0d0d0'\npalette_background = '#101010'\npalette_colors = ['5=#bb78d9', '9=#ff6655']\n",
        )
        .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(
            config.recording.export.cursor,
            crate::RecordingExportCursorMode::On
        );
        assert_eq!(
            config.recording.export.cursor_shape,
            crate::RecordingExportCursorShape::Underline
        );
        assert_eq!(
            config.recording.export.cursor_blink,
            crate::RecordingExportCursorBlinkMode::Off
        );
        assert_eq!(config.recording.export.cursor_blink_period_ms, 650);
        assert_eq!(config.recording.export.cursor_color, "#44aaee");
        assert_eq!(
            config.recording.export.cursor_profile,
            crate::RecordingExportCursorProfile::Ghostty
        );
        assert_eq!(
            config.recording.export.cursor_solid_after_activity_ms,
            Some(900)
        );
        assert_eq!(
            config.recording.export.cursor_solid_after_input_ms,
            Some(910)
        );
        assert_eq!(
            config.recording.export.cursor_solid_after_output_ms,
            Some(920)
        );
        assert_eq!(
            config.recording.export.cursor_solid_after_cursor_ms,
            Some(930)
        );
        assert_eq!(
            config.recording.export.cursor_paint_mode,
            crate::RecordingExportCursorPaintMode::Fill
        );
        assert_eq!(
            config.recording.export.cursor_text_mode,
            crate::RecordingExportCursorTextMode::SwapFgBg
        );
        assert_eq!(config.recording.export.cursor_bar_width_pct, 14);
        assert_eq!(config.recording.export.cursor_underline_height_pct, 11);
        assert_eq!(
            config.recording.export.palette_source,
            crate::RecordingExportPaletteSource::Recording
        );
        assert_eq!(
            config.recording.export.palette_foreground,
            Some("#d0d0d0".to_string())
        );
        assert_eq!(
            config.recording.export.palette_background,
            Some("#101010".to_string())
        );
        assert_eq!(
            config.recording.export.palette_colors,
            vec!["5=#bb78d9".to_string(), "9=#ff6655".to_string()]
        );

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn pane_restore_method_default_is_snapshot() {
        let config = BmuxConfig::default();
        assert_eq!(
            config.behavior.pane_restore_method,
            crate::PaneRestoreMethod::Snapshot
        );
    }

    #[test]
    fn pane_shell_integration_default_is_enabled() {
        let config = BmuxConfig::default();
        assert!(config.behavior.pane_shell_integration);
    }

    #[test]
    fn pane_shell_integration_deserializes_false() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(&path, "[behavior]\npane_shell_integration = false\n")
            .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert!(!config.behavior.pane_shell_integration);

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn pane_shell_integration_defaults_when_missing_from_config() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(&path, "[behavior]\nexit_empty = true\n")
            .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert!(config.behavior.pane_shell_integration);

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn pane_restore_method_deserializes_snapshot() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(&path, "[behavior]\npane_restore_method = \"snapshot\"\n")
            .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(
            config.behavior.pane_restore_method,
            crate::PaneRestoreMethod::Snapshot
        );

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn pane_restore_method_deserializes_retain() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(&path, "[behavior]\npane_restore_method = \"retain\"\n")
            .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(
            config.behavior.pane_restore_method,
            crate::PaneRestoreMethod::Retain
        );

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn pane_restore_method_defaults_when_missing_from_config() {
        let path = temp_config_path();
        let dir = path.parent().expect("temp dir").to_path_buf();
        std::fs::write(&path, "[behavior]\nexit_empty = true\n")
            .expect("failed writing config fixture");

        let config = BmuxConfig::load_from_path(&path).expect("failed loading config");
        assert_eq!(
            config.behavior.pane_restore_method,
            crate::PaneRestoreMethod::Snapshot
        );

        std::fs::remove_dir_all(&dir).expect("failed cleaning temp test directory");
    }

    #[test]
    fn load_merges_discovered_env_and_cli_config_layers_in_precedence_order() {
        let _guard = env_lock().lock().expect("env lock");
        let base_path = temp_config_path();
        let dir = base_path.parent().expect("temp dir").to_path_buf();
        let env_path = dir.join("env-layer.toml");
        let cli_path = dir.join("cli-layer.toml");
        std::fs::write(
            &base_path,
            "[general]\nserver_timeout = 100\n[keybindings.global]\n\"alt+1\" = \"focus_left_pane\"\n",
        )
        .expect("write base config");
        std::fs::write(&env_path, "[general]\nserver_timeout = 200\n").expect("write env config");
        std::fs::write(
            &cli_path,
            "[general]\nserver_timeout = 300\n[keybindings.global]\n\"alt+2\" = \"focus_right_pane\"\n",
        )
        .expect("write cli config");

        let _config_dir_guard =
            EnvVarGuard::set("BMUX_CONFIG_DIR", dir.to_str().expect("temp dir utf-8"));
        let _env_guard = EnvVarGuard::set(BMUX_CONFIG_ENV, env_path.to_str().expect("env utf-8"));
        let _override_guard = push_process_config_overrides(ConfigLoadOverrides {
            base_config_path: None,
            env_config_path: Some(env_path),
            cli_config_path: Some(cli_path),
        });

        let config = BmuxConfig::load().expect("load merged config");
        assert_eq!(config.general.server_timeout, 300);
        assert_eq!(
            config.keybindings.global.get("alt+1"),
            Some(&"focus_left_pane".to_string())
        );
        assert_eq!(
            config.keybindings.global.get("alt+2"),
            Some(&"focus_right_pane".to_string())
        );

        std::fs::remove_dir_all(&dir).expect("cleanup temp dir");
    }

    #[test]
    fn load_resolves_relative_env_and_cli_config_paths_from_cwd() {
        let _guard = env_lock().lock().expect("env lock");
        let base_path = temp_config_path();
        let dir = base_path.parent().expect("temp dir").to_path_buf();
        let env_path = dir.join("relative-env.toml");
        let cli_path = dir.join("relative-cli.toml");
        std::fs::write(&base_path, "[general]\nserver_timeout = 100\n").expect("write base");
        std::fs::write(&env_path, "[general]\nserver_timeout = 150\n").expect("write env");
        std::fs::write(&cli_path, "[general]\nserver_timeout = 175\n").expect("write cli");

        let _cwd_guard = CwdGuard::set(&dir);
        let _config_dir_guard = EnvVarGuard::set("BMUX_CONFIG_DIR", ".");
        let _override_guard = push_process_config_overrides(ConfigLoadOverrides {
            base_config_path: None,
            env_config_path: Some(std::path::PathBuf::from("relative-env.toml")),
            cli_config_path: Some(std::path::PathBuf::from("relative-cli.toml")),
        });

        let config = BmuxConfig::load().expect("load merged config");
        assert_eq!(config.general.server_timeout, 175);

        std::fs::remove_dir_all(&dir).expect("cleanup temp dir");
    }

    #[test]
    fn load_fails_when_config_override_path_is_missing() {
        let _guard = env_lock().lock().expect("env lock");
        let _config_dir_guard = EnvVarGuard::unset("BMUX_CONFIG_DIR");
        let missing = std::env::temp_dir().join(format!(
            "bmux-missing-config-override-{}-{}.toml",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        ));
        let _env_guard = EnvVarGuard::unset(BMUX_CONFIG_ENV);
        let _override_guard = push_process_config_overrides(ConfigLoadOverrides {
            base_config_path: None,
            env_config_path: Some(missing),
            cli_config_path: None,
        });

        let error = BmuxConfig::load().expect_err("missing override should fail");
        assert!(matches!(error, crate::ConfigError::FileNotFound { .. }));
    }

    #[test]
    fn base_config_is_merged_below_primary() {
        let _guard = env_lock().lock().expect("env lock");
        let _config_dir_guard = EnvVarGuard::unset("BMUX_CONFIG_DIR");
        let _env_guard = EnvVarGuard::unset(BMUX_CONFIG_ENV);
        let _no_base_guard = EnvVarGuard::unset("BMUX_NO_BASE_CONFIG");

        let dir = std::env::temp_dir().join(format!(
            "bmux-base-layer-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");

        let base_path = dir.join("base.toml");
        std::fs::write(
            &base_path,
            "[general]\nserver_timeout = 999\nscrollback_limit = 555\n",
        )
        .expect("write base");

        let primary_path = dir.join("bmux.toml");
        std::fs::write(&primary_path, "[general]\nscrollback_limit = 1234\n")
            .expect("write primary");

        // Use the "explicit overrides" path: pass a non-empty overrides set
        // with base + primary, and verify primary wins for shared keys while
        // base provides keys the primary does not set.
        let overrides = ConfigLoadOverrides {
            base_config_path: Some(base_path),
            env_config_path: None,
            cli_config_path: None,
        };
        let config = BmuxConfig::load_from_path_with_overrides(&primary_path, &overrides)
            .expect("load with base");
        // Base-only key survives.
        assert_eq!(config.general.server_timeout, 999);
        // Primary-set key wins.
        assert_eq!(config.general.scrollback_limit, 1234);

        // With BMUX_NO_BASE_CONFIG, base is dropped entirely → server_timeout
        // returns to its default.
        let _no_base = EnvVarGuard::set("BMUX_NO_BASE_CONFIG", "1");
        let config_no_base = BmuxConfig::load_from_path_with_overrides(&primary_path, &overrides)
            .expect("load without base");
        assert_ne!(config_no_base.general.server_timeout, 999);

        std::fs::remove_dir_all(&dir).ok();
    }
}
