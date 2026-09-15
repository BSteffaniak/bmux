#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![cfg_attr(feature = "static-bundled", allow(dead_code))]

use bmux_appearance::{
    RUNTIME_APPEARANCE_STATE_KIND, RuntimeAppearance, RuntimeAppearancePatch,
    RuntimeBorderAppearancePatch, RuntimeContentBlendPatch, RuntimeContentEffectBgPredicate,
    RuntimeContentEffectPatch, RuntimeContentEffectScope, RuntimeStatusAppearancePatch,
};
use bmux_config::{BmuxConfig, ConfigLoadOverrides, ConfigScopeTarget, ScopedConfigLoadRequest};
use bmux_plugin::prompt;
use bmux_plugin::{HostRuntimeApi, ServiceCaller};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    HostConnectionInfo, NativeServiceContext, PluginEvent, PromptEvent, PromptResponse,
    PromptValue, ServiceKind, ServiceResponse, StorageGetRequest, StorageKey, StorageSetRequest,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tracing::{info, warn};

mod control_contract {
    bmux_plugin_schema_macros::schema! { source: "bpdl/theme-plugin.bpdl" }
}

const STORAGE_SELECTED_APPEARANCE: &str = "selected_theme";
// Picker values are namespaced separately from catalog names.
const CONFIGURED_SELECTION: &str = "configured:";

use control_contract::theme_control_v1::Selection as ThemeSelection;

impl ThemeSelection {
    fn picker_value(&self) -> String {
        match self {
            Self::Configured => CONFIGURED_SELECTION.to_string(),
            Self::Preset { name } => format!("preset:{name}"),
        }
    }

    fn from_picker_value(value: &str) -> Option<Self> {
        if value == CONFIGURED_SELECTION {
            Some(Self::Configured)
        } else {
            value
                .strip_prefix("preset:")
                .filter(|name| !name.trim().is_empty())
                .map(|name| Self::Preset {
                    name: name.to_string(),
                })
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedThemeSelection {
    version: u32,
    #[serde(deserialize_with = "required_preset")]
    preset: Option<String>,
}

fn required_preset<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

fn decode_theme_selection(bytes: &[u8]) -> Result<Option<String>, String> {
    let text = std::str::from_utf8(bytes).map_err(|error| error.to_string())?;
    if text.trim_start().starts_with('{') {
        let record: PersistedThemeSelection =
            serde_json::from_str(text).map_err(|error| error.to_string())?;
        if record.version != 1 {
            return Err(format!(
                "unsupported theme selection version {}",
                record.version
            ));
        }
        if record
            .preset
            .as_ref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err("empty persisted preset name".to_string());
        }
        return Ok(record.preset);
    }
    // Legacy records contain an unversioned, literal preset name.
    if text.trim().is_empty() {
        return Err("empty persisted theme selection".to_string());
    }
    Ok(Some(normalized_theme_name(text)))
}

#[derive(Clone)]
struct LiveTheme {
    snapshot: control_contract::theme_control_v1::Snapshot,
    resolved: ResolvedTheme,
}

struct ThemePreview {
    token: u64,
    owner: String,
    original: LiveTheme,
    displayed: ResolvedTheme,
    owners: BTreeSet<String>,
    expires_at: std::time::Instant,
}

fn restore_preview_checkpoint(
    context: &(impl ServiceCaller + Sync),
    active: &ThemePreview,
    config_dirs: &[String],
) -> Result<(), String> {
    reset_removed_providers(context, &active.displayed, &active.original.resolved)?;
    let owners: Vec<_> = active
        .owners
        .iter()
        .filter(|id| id.as_str() != "bmux.decoration")
        .cloned()
        .collect();
    try_apply_theme_extensions(context, &active.original.resolved, &owners, config_dirs)?;
    if active
        .original
        .resolved
        .plugins
        .contains_key("bmux.decoration")
    {
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
        bmux_plugin::block_on_typed_dispatch(
            bmux_decoration_plugin_api::decoration_commands::client::restore_script_state(
                &mut client,
            ),
        )
        .map_err(|error| error.to_string())??;
    } else if active.owners.contains("bmux.decoration") {
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
        bmux_plugin::block_on_typed_dispatch(
            bmux_decoration_plugin_api::decoration_commands::client::apply_theme_extension(
                &mut client,
                String::new(),
                config_dirs.to_vec(),
            ),
        )
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("clearing preview decorations failed: {error:?}"))?;
    }
    Ok(())
}

struct DetachTask(tokio::task::JoinHandle<()>);

impl Drop for DetachTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Default)]
pub struct ThemePlugin {
    lifecycle_context: Option<NativeLifecycleContext>,
    selection: std::sync::Mutex<Option<LiveTheme>>,
    preview: std::sync::Arc<std::sync::Mutex<Option<ThemePreview>>>,
    preview_sequence: std::sync::atomic::AtomicU64,
    recovery_required: std::sync::Arc<std::sync::atomic::AtomicBool>,
    detach_task: Option<DetachTask>,
    refreshed_settings: std::sync::Mutex<Option<toml::Value>>,
}

impl ThemePlugin {
    fn apply_external_form(
        &self,
        context: &NativeServiceContext,
        revision: u64,
        provider_id: &str,
        json: &[u8],
    ) -> Result<control_contract::theme_control_v1::Snapshot, String> {
        if json.len() > 65_536 {
            return Err("settings form exceeds 64 KiB".into());
        }
        let preview = self
            .preview
            .lock()
            .map_err(|_| "preview requires recovery")?;
        if preview.is_some() {
            return Err("theme preview is active".into());
        }
        let mut state = self
            .selection
            .lock()
            .map_err(|_| "theme requires recovery")?;
        let live = state.as_mut().ok_or("read current theme first")?;
        if live.snapshot.revision != revision {
            return Err("stale theme revision".into());
        }
        let next_revision = revision.checked_add(1).ok_or("theme revision exhausted")?;
        if live
            .resolved
            .settings
            .component_settings
            .contains_key(provider_id)
        {
            return Err("use component settings operation".into());
        }
        let provider = live
            .resolved
            .settings
            .providers
            .get(provider_id)
            .ok_or("unknown settings provider")?;
        let endpoint = provider
            .apply_form
            .as_ref()
            .ok_or("provider has no form application contract")?;
        let values: BTreeMap<String, bmux_plugin_sdk::PromptFormValue> =
            serde_json::from_slice(json).map_err(|error| error.to_string())?;
        let settings = self.current_settings(context)?;
        let key = StorageKey::new(provider_storage_key(provider_id, provider))
            .map_err(|error| error.to_string())?;
        let previous = live.resolved.external_payloads.get(provider_id).cloned();
        let outcome =
            call_theme_settings_service::<_, ThemeSettingsPayload>(context, endpoint, &values)
                .map_err(|error| error.to_string());
        let outcome = outcome.and_then(|payload| {
            if matches!(
                settings.persistence,
                ThemePersistence::PersistBetweenConnects
            ) {
                context
                    .storage_set(&StorageSetRequest::new(key, payload.json.clone()))
                    .map_err(|error| error.to_string())?;
            }
            Ok(payload)
        });
        match outcome {
            Ok(payload) => {
                live.resolved
                    .external_payloads
                    .insert(provider_id.to_string(), payload);
            }
            Err(error) => {
                let restored = previous.as_ref().is_some_and(|payload| {
                    provider.apply_settings.is_some()
                        && apply_theme_settings_provider_payload(
                            context,
                            provider_id,
                            provider,
                            payload,
                        )
                        .is_ok()
                });
                if !restored {
                    self.recovery_required
                        .store(true, std::sync::atomic::Ordering::Release);
                }
                return Err(error);
            }
        }
        live.snapshot.revision = next_revision;
        let snapshot = live.snapshot.clone();
        drop(state);
        drop(preview);
        Ok(snapshot)
    }

    fn current_settings(
        &self,
        context: &NativeServiceContext,
    ) -> Result<ThemePluginSettings, String> {
        let refreshed = self
            .refreshed_settings
            .lock()
            .map_err(|_| "theme settings require recovery")?;
        try_parse_settings(refreshed.as_ref().or(context.settings.as_ref()))
    }

    fn set_component_settings(
        &self,
        context: &NativeServiceContext,
        revision: u64,
        provider: &str,
        json: &[u8],
    ) -> Result<control_contract::theme_control_v1::Snapshot, String> {
        if json.len() > 65_536 {
            return Err("theme settings exceed 64 KiB".into());
        }
        let preview = self
            .preview
            .lock()
            .map_err(|_| "preview requires recovery")?;
        if preview.is_some() {
            return Err("theme preview is active".into());
        }
        let mut state = self
            .selection
            .lock()
            .map_err(|_| "theme requires recovery")?;
        let live = state.as_ref().ok_or("read current theme first")?;
        if live.snapshot.revision != revision {
            return Err("stale theme revision".into());
        }
        if !live
            .resolved
            .settings
            .component_settings
            .contains_key(provider)
        {
            return Err("unknown component settings provider".into());
        }
        let value: serde_json::Value =
            serde_json::from_slice(json).map_err(|error| error.to_string())?;
        if !value.is_object() {
            return Err("theme settings must be an object".into());
        }
        let next_revision = revision.checked_add(1).ok_or("theme revision exhausted")?;
        let mut next = live.clone();
        apply_theme_settings_component_overrides(
            &mut next.resolved,
            &BTreeMap::from([(provider.to_string(), json_settings_to_toml(&value))]),
        );
        let settings = self.current_settings(context)?;
        let persistence_key = if matches!(
            settings.persistence,
            ThemePersistence::PersistBetweenConnects
        ) {
            let spec = live
                .resolved
                .settings
                .providers
                .get(provider)
                .ok_or("missing settings provider contract")?;
            Some(
                StorageKey::new(provider_storage_key(provider, spec))
                    .map_err(|error| error.to_string())?,
            )
        } else {
            None
        };
        let owners: Vec<_> = next.resolved.plugins.keys().cloned().collect();
        if let Err(error) = try_apply_theme_extensions(
            context,
            &next.resolved,
            &owners,
            &context.connection.config_dir_candidates,
        )
        .and_then(|()| {
            if let Some(key) = persistence_key {
                context
                    .storage_set(&StorageSetRequest::new(key, json.to_vec()))
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        }) {
            if try_apply_theme_extensions(
                context,
                &live.resolved,
                &owners,
                &context.connection.config_dir_candidates,
            )
            .is_err()
            {
                self.recovery_required
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            return Err(error);
        }
        next.snapshot.revision = next_revision;
        let snapshot = next.snapshot.clone();
        *state = Some(next);
        drop(state);
        drop(preview);
        Ok(snapshot)
    }

    fn reapply_live_theme(
        &self,
        context: &NativeLifecycleContext,
    ) -> Result<(), PluginCommandError> {
        let preview = self
            .preview
            .lock()
            .map_err(|_| PluginCommandError::unavailable("preview requires recovery"))?;
        if preview.is_some() {
            return Ok(());
        }
        let selection = self
            .selection
            .lock()
            .map_err(|_| PluginCommandError::unavailable("theme requires recovery"))?;
        if let Some(live) = selection.as_ref() {
            try_apply_theme_extensions(
                context,
                &live.resolved,
                &live.resolved.plugins.keys().cloned().collect::<Vec<_>>(),
                &context.connection.config_dir_candidates,
            )
            .map_err(PluginCommandError::unavailable)?;
            publish_runtime_appearance(&live.resolved);
        } else {
            apply_configured_theme_extensions(context);
        }
        drop(selection);
        drop(preview);
        Ok(())
    }

    fn watch_detach(&mut self, context: &NativeLifecycleContext) {
        if self.detach_task.is_some() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut events = bmux_plugin::global_event_bus()
            .subscribe::<bmux_clients_plugin_api::clients_events::ClientEvent>(
                &bmux_clients_plugin_api::clients_events::EVENT_KIND,
            )
            .ok();
        let preview = self.preview.clone();
        let recovery = self.recovery_required.clone();
        let context = context.clone();
        self.detach_task = Some(DetachTask(handle.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                if recovery.load(std::sync::atomic::Ordering::Acquire) { break; }
                let owner = tokio::select! {
                    _ = tick.tick() => None,
                    event = async {
                        match events.as_mut() {
                            Some(events) => events.recv().await,
                            None => std::future::pending().await,
                        }
                    } => if let Ok(event) = event {
                        match event.as_ref() {
                            bmux_clients_plugin_api::clients_events::ClientEvent::Detached { client_id } => Some(client_id.to_string()),
                            _ => continue,
                        }
                    } else { events = None; None }
                };
                let Ok(mut guard) = preview.lock() else {
                    recovery.store(true, std::sync::atomic::Ordering::Release);
                    break;
                };
                let Some(active) = guard.as_ref().filter(|active| owner.as_ref() == Some(&active.owner) || std::time::Instant::now() >= active.expires_at) else {
                    continue;
                };
                if restore_preview_checkpoint(&context, active, &context.connection.config_dir_candidates)
                .is_err()
                {
                    recovery.store(true, std::sync::atomic::Ordering::Release);
                    break;
                }
                publish_runtime_appearance(&active.original.resolved);
                *guard = None;
            }
        })));
    }

    fn begin_preview(&self, context: &NativeServiceContext, revision: u64) -> Result<u64, String> {
        let owner = context
            .caller_client_id
            .ok_or("preview requires an attachment")?
            .to_string();
        let mut preview = self
            .preview
            .lock()
            .map_err(|_| "preview requires recovery")?;
        if preview.is_some() {
            return Err("another theme preview is active".into());
        }
        let original = self
            .selection
            .lock()
            .map_err(|_| "theme requires recovery")?
            .clone()
            .ok_or("read current theme first")?;
        if original.snapshot.revision != revision {
            return Err("stale theme revision".into());
        }
        let token = self
            .preview_sequence
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |value| value.checked_add(1),
            )
            .map_err(|_| "preview sequence exhausted")?
            + 1;
        let owners = original.resolved.plugins.keys().cloned().collect();
        if original.resolved.plugins.contains_key("bmux.decoration") {
            let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
            bmux_plugin::block_on_typed_dispatch(
                bmux_decoration_plugin_api::decoration_commands::client::checkpoint_script_state(
                    &mut client,
                ),
            )
            .map_err(|error| error.to_string())??;
        }
        *preview = Some(ThemePreview {
            token,
            owner,
            displayed: original.resolved.clone(),
            original,
            owners,
            expires_at: std::time::Instant::now() + std::time::Duration::from_secs(30),
        });
        drop(preview);
        Ok(token)
    }

    fn renew_preview(&self, context: &NativeServiceContext, token: u64) -> Result<(), String> {
        let owner = context
            .caller_client_id
            .ok_or("preview requires attachment")?
            .to_string();
        let mut preview = self
            .preview
            .lock()
            .map_err(|_| "preview requires recovery")?;
        let active = preview.as_mut().ok_or("no active preview")?;
        if active.owner != owner
            || active.token != token
            || std::time::Instant::now() >= active.expires_at
        {
            return Err("stale or foreign preview".into());
        }
        active.expires_at = std::time::Instant::now() + std::time::Duration::from_secs(30);
        drop(preview);
        Ok(())
    }

    fn preview_theme(
        &self,
        context: &NativeServiceContext,
        token: u64,
        value: &ThemeSelection,
    ) -> Result<(), String> {
        let owner = context
            .caller_client_id
            .ok_or("preview requires an attachment")?
            .to_string();
        let mut preview = self
            .preview
            .lock()
            .map_err(|_| "preview requires recovery")?;
        let active = preview.as_mut().ok_or("no active preview")?;
        if active.token != token
            || active.owner != owner
            || std::time::Instant::now() >= active.expires_at
        {
            return Err("stale or foreign preview".into());
        }
        let settings = self.current_settings(context)?;
        let catalog = load_theme_catalog(&context.connection.config_dir_candidate_paths());
        let theme = resolve_live_selection(context, &catalog, value, &settings)?;
        validate_provider_transition(&active.displayed, &theme)?;
        validate_provider_transition(&theme, &active.original.resolved)?;
        active.owners.extend(theme.plugins.keys().cloned());
        let owners = active.owners.iter().cloned().collect::<Vec<_>>();
        let reset = reset_removed_providers(context, &active.displayed, &theme);
        active.displayed = theme.clone();
        if let Err(error) = reset.and_then(|()| {
            try_apply_theme_extensions(
                context,
                &theme,
                &owners,
                &context.connection.config_dir_candidates,
            )
        }) {
            if let Err(restore) = restore_preview_checkpoint(
                context,
                active,
                &context.connection.config_dir_candidates,
            ) {
                self.recovery_required
                    .store(true, std::sync::atomic::Ordering::Release);
                return Err(format!("{error}; preview recovery failed: {restore}"));
            }
            publish_runtime_appearance(&active.original.resolved);
            *preview = None;
            return Err(error);
        }
        publish_runtime_appearance(&theme);
        drop(preview);
        Ok(())
    }

    fn confirm_preview(
        &self,
        context: &NativeServiceContext,
        token: u64,
        value: ThemeSelection,
    ) -> Result<control_contract::theme_control_v1::Snapshot, String> {
        let owner = context
            .caller_client_id
            .ok_or("preview requires an attachment")?
            .to_string();
        let mut preview = self
            .preview
            .lock()
            .map_err(|_| "preview requires recovery")?;
        let active = preview.as_ref().ok_or("no active preview")?;
        if active.token != token
            || active.owner != owner
            || std::time::Instant::now() >= active.expires_at
        {
            return Err("stale or foreign preview".into());
        }
        // Selection applies all catalog and committed owners. Clear only owners
        // visited during preview that disappeared from both sets.
        let catalog = load_theme_catalog(&context.connection.config_dir_candidate_paths());
        let known: BTreeSet<String> = theme_catalog_plugin_ids(&catalog)
            .into_iter()
            .chain(active.original.resolved.plugins.keys().cloned())
            .collect();
        let removed: Vec<String> = active.owners.difference(&known).cloned().collect();
        let result = try_apply_theme_extensions(
            context,
            &active.original.resolved,
            &removed,
            &context.connection.config_dir_candidates,
        )
        .and_then(|()| {
            self.select_without_preview(context, active.original.snapshot.revision, value)
        });
        if result.is_ok() {
            if active
                .original
                .resolved
                .plugins
                .contains_key("bmux.decoration")
            {
                let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
                let disposal = bmux_plugin::block_on_typed_dispatch(bmux_decoration_plugin_api::decoration_commands::client::discard_script_checkpoint(&mut client)).map_err(|error| error.to_string()).and_then(|result| result);
                if let Err(error) = disposal {
                    *preview = None;
                    self.recovery_required
                        .store(true, std::sync::atomic::Ordering::Release);
                    return Err(format!(
                        "selection committed, but checkpoint disposal requires recovery: {error}"
                    ));
                }
            }
        } else {
            let restoration = restore_preview_checkpoint(
                context,
                active,
                &context.connection.config_dir_candidates,
            );
            if let Err(error) = restoration {
                self.recovery_required
                    .store(true, std::sync::atomic::Ordering::Release);
                return Err(format!(
                    "theme confirmation failed and checkpoint restoration requires recovery: {error}"
                ));
            }
            publish_runtime_appearance(&active.original.resolved);
        }
        *preview = None;
        drop(preview);
        result
    }

    fn cancel_preview(&self, context: &NativeServiceContext, token: u64) -> Result<(), String> {
        let owner = context
            .caller_client_id
            .ok_or("preview requires an attachment")?
            .to_string();
        let mut preview = self
            .preview
            .lock()
            .map_err(|_| "preview requires recovery")?;
        let active = preview.as_ref().ok_or("no active preview")?;
        if active.token != token || active.owner != owner {
            return Err("stale or foreign preview".into());
        }
        restore_preview_checkpoint(context, active, &context.connection.config_dir_candidates)
            .inspect_err(|_| {
                self.recovery_required
                    .store(true, std::sync::atomic::Ordering::Release);
            })?;
        publish_runtime_appearance(&active.original.resolved);
        *preview = None;
        drop(preview);
        Ok(())
    }

    fn refresh_live_theme(
        &self,
        context: &NativeServiceContext,
        expected_revision: u64,
    ) -> Result<control_contract::theme_control_v1::Snapshot, String> {
        let preview_guard = self
            .preview
            .lock()
            .map_err(|_| "preview requires recovery")?;
        if preview_guard.is_some() {
            return Err("theme preview is active".into());
        }
        let result = self.refresh_without_preview(context, expected_revision);
        drop(preview_guard);
        result
    }

    fn refresh_without_preview(
        &self,
        context: &NativeServiceContext,
        expected_revision: u64,
    ) -> Result<control_contract::theme_control_v1::Snapshot, String> {
        let mut state = self
            .selection
            .lock()
            .map_err(|_| "theme state requires recovery")?;
        let current = state
            .as_ref()
            .ok_or("read current theme before refreshing")?;
        if current.snapshot.revision != expected_revision {
            return Err("stale theme revision".into());
        }
        let revision = expected_revision
            .checked_add(1)
            .ok_or("theme revision exhausted")?;
        let config_path = context
            .connection
            .probe_config_file("bmux.toml")
            .unwrap_or_else(|| config_paths_from_connection(&context.connection).config_file());
        let config = BmuxConfig::load_from_path_with_overrides(
            &config_path,
            &ConfigLoadOverrides::for_process(),
        )
        .map_err(|error| error.to_string())?;
        let mut refreshed = self
            .refreshed_settings
            .lock()
            .map_err(|_| "theme settings require recovery")?;
        let settings_value = config
            .plugins
            .settings
            .get("bmux.theme")
            .cloned()
            .unwrap_or_else(|| toml::Value::Table(toml::Table::new()));
        let settings: ThemePluginSettings = config
            .plugins
            .settings
            .get("bmux.theme")
            .cloned()
            .map(toml::Value::try_into)
            .transpose()
            .map_err(|error| format!("invalid theme settings: {error}"))?
            .unwrap_or_default();
        let catalog = load_theme_catalog(&context.connection.config_dir_candidate_paths());
        let theme =
            resolve_live_selection(context, &catalog, &current.snapshot.selection, &settings)?;
        validate_provider_transition(&current.resolved, &theme)?;
        validate_provider_transition(&theme, &current.resolved)?;
        let owners: Vec<String> = current
            .resolved
            .plugins
            .keys()
            .chain(theme.plugins.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if let Err(error) =
            reset_removed_providers(context, &current.resolved, &theme).and_then(|()| {
                try_apply_theme_extensions(
                    context,
                    &theme,
                    &owners,
                    &context.connection.config_dir_candidates,
                )
            })
        {
            reset_removed_providers(context, &theme, &current.resolved)
                .and_then(|()| {
                    try_apply_theme_extensions(
                        context,
                        &current.resolved,
                        &owners,
                        &context.connection.config_dir_candidates,
                    )
                })
                .map_err(|restore| {
                    self.recovery_required
                        .store(true, std::sync::atomic::Ordering::Release);
                    format!("{error}; recovery failed: {restore}")
                })?;
            return Err(error);
        }
        *refreshed = Some(settings_value);
        drop(refreshed);
        let snapshot = control_contract::theme_control_v1::Snapshot {
            revision,
            selection: current.snapshot.selection.clone(),
        };
        publish_runtime_appearance(&theme);
        *state = Some(LiveTheme {
            snapshot: snapshot.clone(),
            resolved: theme,
        });
        drop(state);
        Ok(snapshot)
    }

    fn select_live_theme(
        &self,
        context: &NativeServiceContext,
        expected_revision: u64,
        value: ThemeSelection,
    ) -> Result<control_contract::theme_control_v1::Snapshot, String> {
        let preview_guard = self
            .preview
            .lock()
            .map_err(|_| "preview requires recovery")?;
        if preview_guard.is_some() {
            return Err("theme preview is active".into());
        }
        let result = self.select_without_preview(context, expected_revision, value);
        drop(preview_guard);
        result
    }

    fn select_without_preview(
        &self,
        context: &NativeServiceContext,
        expected_revision: u64,
        value: ThemeSelection,
    ) -> Result<control_contract::theme_control_v1::Snapshot, String> {
        let mut state = self
            .selection
            .lock()
            .map_err(|_| "theme state requires recovery")?;
        let current = state
            .as_ref()
            .ok_or("read current theme before selecting")?;
        if current.snapshot.revision != expected_revision {
            return Err("stale theme revision".into());
        }
        let revision = current
            .snapshot
            .revision
            .checked_add(1)
            .ok_or("theme revision exhausted")?;
        let settings = self.current_settings(context)?;
        let catalog = load_theme_catalog(&context.connection.config_dir_candidate_paths());
        let theme = resolve_live_selection(context, &catalog, &value, &settings)?;
        validate_provider_transition(&current.resolved, &theme)?;
        validate_provider_transition(&theme, &current.resolved)?;
        let owners: Vec<String> = theme_catalog_plugin_ids(&catalog)
            .into_iter()
            .chain(current.resolved.plugins.keys().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let apply = reset_removed_providers(context, &current.resolved, &theme)
            .and_then(|()| {
                try_apply_theme_extensions(
                    context,
                    &theme,
                    &owners,
                    &context.connection.config_dir_candidates,
                )
            })
            .and_then(|()| {
                if matches!(
                    settings.persistence,
                    ThemePersistence::PersistBetweenConnects
                ) {
                    persist_theme_name(context, &value.picker_value())
                } else {
                    Ok(())
                }
            });
        if let Err(error) = apply {
            if let Err(restore_error) = reset_removed_providers(context, &theme, &current.resolved)
                .and_then(|()| {
                    try_apply_theme_extensions(
                        context,
                        &current.resolved,
                        &owners,
                        &context.connection.config_dir_candidates,
                    )
                })
            {
                self.recovery_required
                    .store(true, std::sync::atomic::Ordering::Release);
                return Err(format!("{error}; theme recovery failed: {restore_error}"));
            }
            return Err(error);
        }
        publish_runtime_appearance(&theme);
        let snapshot = control_contract::theme_control_v1::Snapshot {
            revision,
            selection: value,
        };
        *state = Some(LiveTheme {
            snapshot: snapshot.clone(),
            resolved: theme,
        });
        drop(state);
        Ok(snapshot)
    }
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
        self.watch_detach(&context);
        apply_configured_theme_extensions(&context);
        Ok(EXIT_OK)
    }

    fn handle_event(&mut self, event: PluginEvent) -> Result<i32, PluginCommandError> {
        if self.detach_task.is_none()
            && let Some(context) = self.lifecycle_context.clone()
        {
            self.watch_detach(&context);
        }
        if event.kind.as_str() == "bmux.core/server_started"
            && let Some(context) = self.lifecycle_context.as_ref()
        {
            info!(
                data_dir = %context.connection.data_dir,
                event_kind = %event.kind.as_str(),
                "theme plugin handling lifecycle event; reapplying theme state",
            );
            self.reapply_live_theme(context)?;
        }
        Ok(EXIT_OK)
    }

    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        bmux_plugin_sdk::route_command!(context, {
            "pick-theme" => pick_theme(&context),
            "refresh-theme" => refresh_theme(&context),
        })
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        if self
            .recovery_required
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return ServiceResponse::error(
                "theme_recovery_required",
                "theme rollback failed; restart or repair the theme owner before further operations",
            );
        }
        bmux_plugin_sdk::route_service!(context, {
            "theme-control-v1", "set-component-settings" => |req: control_contract::theme_control_v1::client::SetComponentSettingsRequest, ctx| {
                Ok::<_, ServiceResponse>(self.set_component_settings(ctx, req.expected_revision, &req.provider, &req.json))
            },
            "theme-control-v1", "apply-external-form" => |req: control_contract::theme_control_v1::client::ApplyExternalFormRequest, ctx| {
                Ok::<_, ServiceResponse>(self.apply_external_form(ctx, req.expected_revision, &req.provider, &req.json))
            },
            "theme-control-v1", "current" => |_req: (), ctx| {
                let mut selection = self.selection.lock().map_err(|_| ServiceResponse::error("theme_state_poisoned", "theme selection requires recovery"))?;
                if selection.is_none() {
                    let settings = try_parse_settings(ctx.settings.as_ref()).map_err(|error| ServiceResponse::error("invalid_theme_settings", error))?;
                    let catalog = load_theme_catalog(&ctx.connection.config_dir_candidate_paths());
                    let active = active_theme_stack(ctx, &settings, &catalog).ok_or_else(|| ServiceResponse::error("theme_not_found", "theme selection requires recovery"))?;
                    let value = ThemeSelection::from_picker_value(&picker_selection_name(&active)).ok_or_else(|| ServiceResponse::error("invalid_selection", "invalid startup selection"))?;
                    let resolved = resolve_live_selection(ctx, &catalog, &value, &settings).map_err(|error| ServiceResponse::error("theme_not_found", error))?;
                    *selection = Some(LiveTheme { snapshot: control_contract::theme_control_v1::Snapshot { revision: 0, selection: value }, resolved });
                }
                Ok::<_, ServiceResponse>(selection.as_ref().expect("initialized selection").snapshot.clone())
            },
            "theme-control-v1", "begin-preview" => |req: control_contract::theme_control_v1::client::BeginPreviewRequest, ctx| {
                Ok::<_, ServiceResponse>(self.begin_preview(ctx, req.expected_revision))
            },
            "theme-control-v1", "renew-preview" => |req: control_contract::theme_control_v1::client::RenewPreviewRequest, ctx| {
                Ok::<_, ServiceResponse>(self.renew_preview(ctx, req.token))
            },
            "theme-control-v1", "preview" => |req: control_contract::theme_control_v1::client::PreviewRequest, ctx| {
                Ok::<_, ServiceResponse>(self.preview_theme(ctx, req.token, &req.selection))
            },
            "theme-control-v1", "confirm-preview" => |req: control_contract::theme_control_v1::client::ConfirmPreviewRequest, ctx| {
                Ok::<_, ServiceResponse>(self.confirm_preview(ctx, req.token, req.selection))
            },
            "theme-control-v1", "cancel-preview" => |req: control_contract::theme_control_v1::client::CancelPreviewRequest, ctx| {
                Ok::<_, ServiceResponse>(self.cancel_preview(ctx, req.token))
            },
            "theme-control-v1", "refresh" => |req: control_contract::theme_control_v1::client::RefreshRequest, ctx| {
                Ok::<_, ServiceResponse>(self.refresh_live_theme(ctx, req.expected_revision))
            },
            "theme-control-v1", "select" => |req: control_contract::theme_control_v1::client::SelectRequest, ctx| {
                let result = self.select_live_theme(ctx, req.expected_revision, req.selection);
                Ok::<_, ServiceResponse>(result)
            },
            "theme-state", "active-appearance" => |_req: (), ctx| {
                let appearance = active_runtime_appearance(ctx).ok_or_else(|| {
                    ServiceResponse::error("theme_not_found", "active theme was not found")
                })?;
                info!("active runtime appearance service returned resolved theme appearance");
                Ok(appearance)
            },
            "theme-state", "active-appearance-for-cwd" => |req: ActiveAppearanceForCwdArgs, ctx| {
                let appearance = active_runtime_appearance_for_cwd(ctx, &req.cwd).ok_or_else(|| {
                    ServiceResponse::error("theme_not_found", "active theme was not found for cwd")
                })?;
                info!(cwd = %req.cwd, "active runtime appearance service returned cwd-scoped theme appearance");
                Ok(appearance)
            },
            "theme-state", "active-appearance-for-scope" => |req: ActiveAppearanceForScopeArgs, ctx| {
                let target = ConfigScopeTarget {
                    name: req.scope,
                    attributes: req.attributes,
                };
                let appearance = active_runtime_appearance_for_scope(ctx, target).ok_or_else(|| {
                    ServiceResponse::error("theme_not_found", "active theme was not found for scope")
                })?;
                info!("active runtime appearance service returned scope-scoped theme appearance");
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
    #[serde(skip_serializing_if = "ThemeSettingsConfig::is_empty")]
    settings: ThemeSettingsConfig,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ThemeSettingsConfig {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    providers: BTreeMap<String, ThemeSettingsProviderSpec>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    component_settings: BTreeMap<String, ThemeComponentSettingsSpec>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    forms: BTreeMap<String, ThemeSettingsFormSpec>,
}

impl ThemeSettingsConfig {
    fn is_empty(&self) -> bool {
        self.providers.is_empty() && self.component_settings.is_empty() && self.forms.is_empty()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ThemeComponentSettingsSpec {
    components: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ThemeSettingsFormSpec {
    title: Option<String>,
    section_label: Option<String>,
    width_min: Option<u16>,
    width_max: Option<u16>,
    fields: Vec<ThemeSettingsFormFieldSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ThemeSettingsFormFieldSpec {
    key: String,
    label: String,
    #[serde(rename = "type")]
    field_type: ThemeSettingsFormFieldType,
    default: Option<toml::Value>,
    min: Option<i64>,
    max: Option<i64>,
    placeholder: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ThemeSettingsFormFieldType {
    Bool,
    #[default]
    Text,
    Integer,
    Number,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ThemeSettingsProviderSpec {
    modal_id: Option<String>,
    storage_key: Option<String>,
    prompt_on_select: Option<bool>,
    form: Option<ThemeSettingsEndpoint>,
    apply_form: Option<ThemeSettingsEndpoint>,
    apply_settings: Option<ThemeSettingsEndpoint>,
    reset_settings: Option<ThemeSettingsEndpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ThemeSettingsEndpoint {
    capability: String,
    interface_id: String,
    operation: String,
    #[serde(default)]
    kind: ThemeSettingsServiceKind,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ThemeSettingsServiceKind {
    #[default]
    Query,
    Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ThemeSettingsPayload {
    json: Vec<u8>,
}

impl ThemeSettingsPayload {
    fn from_value(value: &serde_json::Value) -> Option<Self> {
        serde_json::to_vec(value).ok().map(|json| Self { json })
    }
}

impl From<ThemeSettingsServiceKind> for ServiceKind {
    fn from(kind: ThemeSettingsServiceKind) -> Self {
        match kind {
            ThemeSettingsServiceKind::Query => Self::Query,
            ThemeSettingsServiceKind::Command => Self::Command,
        }
    }
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
            settings: ThemeSettingsConfig::default(),
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
    appearance_themes: Vec<String>,
    #[serde(default)]
    component_themes: Vec<String>,
    #[serde(default)]
    persistence: ThemePersistence,
    #[serde(default)]
    components: BTreeMap<String, toml::Value>,
    #[serde(default)]
    component_targets: BTreeMap<String, toml::Value>,
    #[serde(default)]
    theme_settings: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone)]
struct ThemeCatalogEntry {
    name: String,
    theme: ThemeConfig,
}

#[derive(Debug, Clone)]
struct ResolvedTheme {
    external_payloads: BTreeMap<String, ThemeSettingsPayload>,
    appearance: RuntimeAppearance,
    plugins: BTreeMap<String, toml::Value>,
    settings: ThemeSettingsConfig,
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

#[derive(Debug, Serialize, Deserialize)]
struct ActiveAppearanceForCwdArgs {
    cwd: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ActiveAppearanceForScopeArgs {
    scope: String,
    attributes: BTreeMap<String, String>,
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

fn refresh_theme(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let current = bmux_plugin::block_on_typed_dispatch(
        control_contract::theme_control_v1::client::current(&mut client),
    )
    .map_err(|error| PluginCommandError::unavailable(error.to_string()))?;
    bmux_plugin::block_on_typed_dispatch(control_contract::theme_control_v1::client::refresh(
        &mut client,
        current.revision,
    ))
    .map_err(|error| PluginCommandError::unavailable(error.to_string()))?
    .map_err(PluginCommandError::unavailable)?;
    Ok(EXIT_OK)
}

fn pick_theme(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let handle = tokio::runtime::Handle::try_current().map_err(|_| {
        PluginCommandError::unavailable("no tokio runtime available; theme picker requires attach")
    })?;
    handle.spawn(run_theme_picker(context.clone()));
    Ok(EXIT_OK)
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
    if let Err(error) = try_apply_theme_extensions(
        context,
        &active.theme,
        &all_plugin_ids,
        &context.connection.config_dir_candidates,
    ) {
        warn!(%error, "startup theme failed; retaining previous appearance");
        return;
    }
    log_active_theme(context, &active);
    publish_runtime_appearance(&active.theme);
}

fn active_runtime_appearance(
    context: &(impl ThemeHostContext + ?Sized),
) -> Option<RuntimeAppearance> {
    // Queries must agree with the appearance already published to attachments.
    // Configuration is only the bootstrap fallback before a retained value exists.
    if let Ok((appearance, _)) = bmux_plugin::global_event_bus()
        .subscribe_state::<RuntimeAppearance>(&RUNTIME_APPEARANCE_STATE_KIND)
    {
        return Some((*appearance).clone());
    }
    configured_theme(context).map(|active| active.theme.appearance)
}

fn active_runtime_appearance_for_cwd(
    context: &(impl ThemeHostContext + ?Sized),
    cwd: &str,
) -> Option<RuntimeAppearance> {
    active_runtime_appearance_for_scope(context, ConfigScopeTarget::with_cwd("pane", cwd))
}

fn active_runtime_appearance_for_scope(
    context: &(impl ThemeHostContext + ?Sized),
    target: ConfigScopeTarget,
) -> Option<RuntimeAppearance> {
    let scoped_settings = scoped_theme_settings_for_target(context, target).ok()?;
    configured_theme_with_settings(context, &scoped_settings).map(|active| active.theme.appearance)
}

fn configured_theme(context: &(impl ThemeHostContext + ?Sized)) -> Option<ActiveThemeResolution> {
    let settings = match try_parse_settings(context.settings_value()) {
        Ok(settings) => settings,
        Err(error) => {
            warn!(%error, "theme configuration requires recovery");
            return None;
        }
    };
    configured_theme_with_settings(context, &settings)
}

fn configured_theme_with_settings(
    context: &(impl ThemeHostContext + ?Sized),
    settings: &ThemePluginSettings,
) -> Option<ActiveThemeResolution> {
    let catalog = load_theme_catalog(&context.connection_info().config_dir_candidate_paths());
    info!(
        data_dir = %context.connection_info().data_dir,
        catalog_count = catalog.len(),
        configured_theme = settings.theme.as_deref().unwrap_or(""),
        configured_stack = ?settings.themes,
        persistence = ?settings.persistence,
        "theme settings parsed",
    );
    let active = active_theme_stack(context, settings, &catalog)?;
    let theme = if active.source == ActiveThemeSource::Persisted {
        resolve_live_selection(
            context,
            &catalog,
            &ThemeSelection::Preset {
                name: active.requested_name.clone()?,
            },
            settings,
        )
        .inspect_err(|error| warn!(%error, "persisted theme settings require recovery"))
        .ok()?
    } else {
        resolve_theme_stack_with_settings(&catalog, &active.stack, settings)?
    };
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

fn picker_active_name(stack: &[String]) -> String {
    stack
        .iter()
        .find(|name| name.as_str() != "mode-aware")
        .cloned()
        .unwrap_or_else(|| "default".to_string())
}

fn picker_selection_name(active: &ActiveThemeStack) -> String {
    if active.source == ActiveThemeSource::Persisted {
        ThemeSelection::Preset {
            name: picker_active_name(&active.stack),
        }
        .picker_value()
    } else {
        CONFIGURED_SELECTION.to_string()
    }
}

fn picker_request(
    options: Vec<bmux_plugin_sdk::PromptOption>,
    index: usize,
) -> bmux_plugin_sdk::PromptRequest {
    bmux_plugin_sdk::PromptRequest::single_select("Select Theme", options)
        .message("Move to preview live. Enter applies. Esc restores previous theme.")
        .single_default_index(index)
        .single_live_preview(true)
        .policy(bmux_plugin_sdk::PromptPolicy::RejectIfBusy)
        .width_range(48, 96)
}

struct PickerPreviewGuard {
    context: NativeCommandContext,
    token: Option<u64>,
}

impl Drop for PickerPreviewGuard {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            // The ServiceCaller adapter completes synchronously, so cleanup does
            // not depend on spawning a task into a runtime that is shutting down.
            let mut client = bmux_plugin::ServiceCallerDispatchClient::new(&self.context);
            let result = bmux_plugin::block_on_typed_dispatch(
                control_contract::theme_control_v1::client::cancel_preview(&mut client, token),
            );
            if !matches!(result, Ok(Ok(()))) {
                warn!(?result, "cancelled picker preview requires recovery");
            }
        }
    }
}

async fn cancel_picker_preview(context: &NativeCommandContext, token: u64) {
    let mut control = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let cleanup =
        control_contract::theme_control_v1::client::cancel_preview(&mut control, token).await;
    if !matches!(cleanup, Ok(Ok(()))) {
        warn!(?cleanup, "theme restoration failed");
    }
}

async fn confirm_picker_selection(
    context: &NativeCommandContext,
    token: Option<u64>,
    revision: u64,
    selection: ThemeSelection,
) -> bool {
    let mut control = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let result = if let Some(token) = token {
        control_contract::theme_control_v1::client::confirm_preview(&mut control, token, selection)
            .await
    } else {
        control_contract::theme_control_v1::client::select(&mut control, revision, selection).await
    };
    if !matches!(result, Ok(Ok(_))) {
        warn!(?result, "theme confirmation failed");
        return false;
    }
    true
}

async fn run_theme_picker(context: NativeCommandContext) {
    let Ok(settings) = try_parse_settings(context.settings.as_ref())
        .inspect_err(|error| warn!(%error, "cannot open theme picker"))
    else {
        return;
    };
    let catalog = load_theme_catalog(&context.connection.config_dir_candidate_paths());

    let mut control = bmux_plugin::ServiceCallerDispatchClient::new(&context);
    let current = match control_contract::theme_control_v1::client::current(&mut control).await {
        Ok(current) => current,
        Err(error) => {
            warn!(%error, "theme control unavailable");
            return;
        }
    };
    let active_name = current.selection.picker_value();
    let all_plugin_ids = theme_catalog_plugin_ids(&catalog);

    let request = picker_request(
        picker_options(&catalog, &settings, &active_name),
        selected_index(&catalog, &active_name),
    );

    let Ok((mut response_rx, mut event_rx)) = prompt::submit_with_events(request) else {
        warn!("theme picker prompt host unavailable");
        return;
    };

    let mut preview_guard = PickerPreviewGuard {
        context: context.clone(),
        token: None,
    };
    let mut preview_token = None;
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(5));
    let mut events_open = true;
    let selected_name = loop {
        tokio::select! {
            biased;
            response = &mut response_rx => {
                break match response {
                    Ok(PromptResponse::Submitted(PromptValue::Single(name))) => Some(name),
                    Ok(PromptResponse::Cancelled | PromptResponse::RejectedBusy | PromptResponse::Submitted(_)) | Err(_) => None,
                };
            }
            _ = heartbeat.tick() => {
                if let Some(token) = preview_token
                    && !matches!(control_contract::theme_control_v1::client::renew_preview(&mut control, token).await, Ok(Ok(()))) {
                    break None;
                }
            }
            event = event_rx.recv(), if events_open => {
                let Some(event) = event else {
                    events_open = false;
                    continue;
                };
                if let PromptEvent::SelectionChanged { value, .. } = event
                    && let Some(selection) = ThemeSelection::from_picker_value(&value)
                {
                    if preview_token.is_none() {
                        match control_contract::theme_control_v1::client::begin_preview(&mut control, current.revision).await {
                            Ok(Ok(token)) => { preview_token = Some(token); preview_guard.token = Some(token); },
                            error => { warn!(?error, "cannot begin theme preview"); break None; }
                        }
                    }
                    let token = preview_token.expect("preview acquired");
                    if !matches!(control_contract::theme_control_v1::client::preview(&mut control, token, selection).await, Ok(Ok(()))) {
                        break None;
                    }
                }
            }
        }
    };

    if let Some(name) = selected_name
        && let Some(theme) = resolve_picker_value(&catalog, &name, &settings)
    {
        let selection = ThemeSelection::from_picker_value(&name).expect("resolved picker value");
        if !confirm_picker_selection(&context, preview_token, current.revision, selection).await {
            if let Some(token) = preview_token {
                cancel_picker_preview(&context, token).await;
                preview_guard.token = None;
            }
            return;
        }
        preview_guard.token = None;
        if name != CONFIGURED_SELECTION {
            configure_theme_settings_providers(&context, &theme, &settings, &all_plugin_ids).await;
        }
        info!(theme = %name, persistence = ?settings.persistence, "theme selected");
        return;
    }

    if let Some(token) = preview_token {
        preview_guard.token = None;
        let cleanup =
            control_contract::theme_control_v1::client::cancel_preview(&mut control, token).await;
        if !matches!(cleanup, Ok(Ok(()))) {
            warn!(?cleanup, "theme restoration failed");
        }
    }
}

async fn configure_theme_settings_providers(
    context: &NativeCommandContext,
    theme: &ResolvedTheme,
    settings: &ThemePluginSettings,
    all_plugin_ids: &[String],
) {
    for (provider_id, provider) in &theme.settings.providers {
        if !provider.prompt_on_select.unwrap_or(false) {
            continue;
        }
        configure_theme_settings_provider(
            context,
            theme,
            provider_id,
            provider,
            settings,
            all_plugin_ids,
        )
        .await;
    }
}

async fn submit_external_settings(
    context: &NativeCommandContext,
    revision: u64,
    provider_id: &str,
    values: &BTreeMap<String, bmux_plugin_sdk::PromptFormValue>,
) {
    let Ok(json) = serde_json::to_vec(values) else {
        return;
    };
    let mut control = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let result = control_contract::theme_control_v1::client::apply_external_form(
        &mut control,
        revision,
        provider_id.to_string(),
        json,
    )
    .await;
    if !matches!(result, Ok(Ok(_))) {
        warn!(?result, provider_id, "external settings update failed");
    }
}

async fn configure_theme_settings_provider(
    context: &NativeCommandContext,
    theme: &ResolvedTheme,
    provider_id: &str,
    provider: &ThemeSettingsProviderSpec,
    settings: &ThemePluginSettings,
    _all_plugin_ids: &[String],
) {
    let mut control = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let Ok(snapshot) = control_contract::theme_control_v1::client::current(&mut control)
        .await
        .inspect_err(|error| warn!(%error, "cannot read settings revision"))
    else {
        return;
    };
    let form_revision = snapshot.revision;
    let revision = theme
        .settings
        .component_settings
        .contains_key(provider_id)
        .then_some(form_revision);
    let defaults =
        effective_theme_settings_payload(context, theme, provider_id, provider, settings);
    let request = if let Some(form_endpoint) = provider.form.as_ref() {
        match call_theme_settings_service::<_, bmux_plugin_sdk::PromptRequest>(
            context,
            form_endpoint,
            &defaults,
        ) {
            Ok(request) => request,
            Err(error) => {
                warn!(%error, provider_id, "failed building theme settings form");
                return;
            }
        }
    } else if let Some(form) = theme.settings.forms.get(provider_id) {
        build_builtin_theme_settings_form(provider_id, form, &defaults)
    } else {
        return;
    };
    let request = request
        .owner_plugin_id("bmux.theme")
        .modal_id(
            provider
                .modal_id
                .clone()
                .unwrap_or_else(|| format!("theme-settings-{provider_id}")),
        )
        .policy(bmux_plugin_sdk::PromptPolicy::Enqueue);
    let response = match prompt::request(request).await {
        Ok(response) => response,
        Err(error) => {
            warn!(%error, provider_id, "failed opening theme settings form");
            return;
        }
    };
    let PromptResponse::Submitted(PromptValue::Form(values)) = response else {
        return;
    };
    if revision.is_none() && provider.apply_form.is_some() {
        submit_external_settings(context, form_revision, provider_id, &values).await;
        return;
    }
    let settings_payload = if let Some(apply_endpoint) = provider.apply_form.as_ref() {
        match call_theme_settings_service::<_, ThemeSettingsPayload>(
            context,
            apply_endpoint,
            &values,
        ) {
            Ok(settings_payload) => settings_payload,
            Err(error) => {
                warn!(%error, provider_id, "failed applying theme settings form");
                return;
            }
        }
    } else if let Some(form) = theme.settings.forms.get(provider_id) {
        builtin_theme_settings_payload_from_form(form, &values)
    } else {
        return;
    };
    if let Some(revision) = revision {
        let result = control_contract::theme_control_v1::client::set_component_settings(
            &mut control,
            revision,
            provider_id.to_string(),
            settings_payload.json,
        )
        .await;
        if !matches!(result, Ok(Ok(_))) {
            warn!(?result, provider_id, "component settings update failed");
        }
        return;
    }
    if matches!(
        settings.persistence,
        ThemePersistence::PersistBetweenConnects
    ) {
        persist_theme_settings(context, provider_id, provider, &settings_payload);
    } else {
        info!(
            provider_id,
            persistence = ?settings.persistence,
            "theme settings not persisted because persistence is disabled",
        );
    }
}

fn apply_theme_settings_provider_payload(
    context: &impl ServiceCaller,
    provider_id: &str,
    provider: &ThemeSettingsProviderSpec,
    payload: &ThemeSettingsPayload,
) -> Result<(), String> {
    let Some(endpoint) = provider.apply_settings.as_ref() else {
        return Err(format!(
            "settings provider {provider_id} has no apply_settings contract"
        ));
    };
    call_theme_settings_service::<_, ThemeSettingsPayload>(context, endpoint, payload)
        .map(|_| ())
        .map_err(|error| format!("settings provider {provider_id}: {error}"))
}

fn effective_theme_settings_payload(
    context: &impl ServiceCaller,
    theme: &ResolvedTheme,
    provider_id: &str,
    provider: &ThemeSettingsProviderSpec,
    settings: &ThemePluginSettings,
) -> ThemeSettingsPayload {
    let base = if theme.settings.component_settings.contains_key(provider_id) {
        component_theme_settings_defaults(theme, provider_id)
    } else {
        ThemeSettingsPayload { json: Vec::new() }
    };
    let overlay = effective_theme_settings_overrides(context, provider_id, provider, settings);
    merge_theme_settings_payloads(&base, &overlay)
}

fn effective_theme_settings_overrides(
    context: &(impl ServiceCaller + ?Sized),
    provider_id: &str,
    provider: &ThemeSettingsProviderSpec,
    settings: &ThemePluginSettings,
) -> ThemeSettingsPayload {
    if let Some(persisted) = read_persisted_theme_settings(context, provider_id, provider) {
        return persisted;
    }
    if let Some(configured) = settings.theme_settings.get(provider_id)
        && let Ok(value) = serde_json::to_value(configured)
        && let Some(payload) = ThemeSettingsPayload::from_value(&value)
    {
        return payload;
    }
    ThemeSettingsPayload { json: Vec::new() }
}

fn merge_theme_settings_payloads(
    base: &ThemeSettingsPayload,
    overlay: &ThemeSettingsPayload,
) -> ThemeSettingsPayload {
    let base_value = payload_to_json_value(base);
    let overlay_value = payload_to_json_value(overlay);
    let merged = match (base_value, overlay_value) {
        (serde_json::Value::Object(mut base), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                base.insert(key, value);
            }
            serde_json::Value::Object(base)
        }
        (_, serde_json::Value::Object(overlay)) if overlay.is_empty() => {
            payload_to_json_value(base)
        }
        (_, overlay) => overlay,
    };
    ThemeSettingsPayload::from_value(&merged).unwrap_or(ThemeSettingsPayload { json: Vec::new() })
}

fn component_theme_settings_defaults(
    theme: &ResolvedTheme,
    provider_id: &str,
) -> ThemeSettingsPayload {
    let mut defaults = serde_json::Map::new();
    let Some(spec) = theme.settings.component_settings.get(provider_id) else {
        return ThemeSettingsPayload { json: Vec::new() };
    };
    let Some(components) = theme
        .plugins
        .get("bmux.decoration")
        .and_then(toml::Value::as_table)
        .and_then(|extension| extension.get("components"))
        .and_then(toml::Value::as_table)
    else {
        return ThemeSettingsPayload { json: Vec::new() };
    };
    for component_id in &spec.components {
        let Some(settings) = components
            .get(component_id)
            .and_then(toml::Value::as_table)
            .and_then(|component| component.get("settings"))
            .and_then(toml::Value::as_table)
        else {
            continue;
        };
        for (key, value) in settings {
            defaults
                .entry(key.clone())
                .or_insert_with(|| toml_to_json_value(value));
        }
    }
    ThemeSettingsPayload::from_value(&serde_json::Value::Object(defaults))
        .unwrap_or(ThemeSettingsPayload { json: Vec::new() })
}

fn read_persisted_theme_settings(
    context: &(impl ServiceCaller + ?Sized),
    provider_id: &str,
    provider: &ThemeSettingsProviderSpec,
) -> Option<ThemeSettingsPayload> {
    let key = provider_storage_key(provider_id, provider);
    let request = StorageGetRequest::new(storage_key_from_string(&key));
    let response = context.storage_get(&request).ok()?;
    response.value.map(|json| ThemeSettingsPayload { json })
}

fn payload_to_json_value(payload: &ThemeSettingsPayload) -> serde_json::Value {
    if payload.json.is_empty() {
        return serde_json::Value::Object(serde_json::Map::new());
    }
    serde_json::from_slice(&payload.json)
        .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()))
}

fn build_builtin_theme_settings_form(
    provider_id: &str,
    form: &ThemeSettingsFormSpec,
    defaults: &ThemeSettingsPayload,
) -> bmux_plugin_sdk::PromptRequest {
    let defaults = payload_to_json_value(defaults);
    let fields = form
        .fields
        .iter()
        .map(|field| builtin_theme_settings_form_field(field, &defaults))
        .collect();
    let title = form
        .title
        .clone()
        .unwrap_or_else(|| format!("{provider_id} Settings"));
    let section_label = form.section_label.clone().unwrap_or_else(|| title.clone());
    let mut request = bmux_plugin_sdk::PromptRequest::form(
        title,
        vec![bmux_plugin_sdk::PromptFormSection::new(
            provider_id,
            section_label,
            fields,
        )],
    );
    if let (Some(min), Some(max)) = (form.width_min, form.width_max) {
        request = request.width_range(min, max);
    }
    request
}

fn builtin_theme_settings_form_field(
    field: &ThemeSettingsFormFieldSpec,
    defaults: &serde_json::Value,
) -> bmux_plugin_sdk::PromptFormField {
    let default = defaults
        .get(&field.key)
        .map(json_to_toml_value)
        .or_else(|| field.default.clone());
    let kind = match field.field_type {
        ThemeSettingsFormFieldType::Bool => bmux_plugin_sdk::PromptFormFieldKind::Bool {
            default: default
                .as_ref()
                .and_then(toml_value_as_bool)
                .unwrap_or(false),
        },
        ThemeSettingsFormFieldType::Text => bmux_plugin_sdk::PromptFormFieldKind::Text {
            initial_value: default
                .as_ref()
                .map(component_setting_string)
                .unwrap_or_default(),
            placeholder: field.placeholder.clone(),
            validation: None,
        },
        ThemeSettingsFormFieldType::Integer => bmux_plugin_sdk::PromptFormFieldKind::Integer {
            initial_value: default
                .as_ref()
                .and_then(toml::Value::as_integer)
                .unwrap_or(0),
            min: field.min,
            max: field.max,
        },
        ThemeSettingsFormFieldType::Number => bmux_plugin_sdk::PromptFormFieldKind::Number {
            initial_value: default
                .as_ref()
                .map(component_setting_string)
                .unwrap_or_default(),
            min: field.min.map(|value| value.to_string()),
            max: field.max.map(|value| value.to_string()),
        },
    };
    bmux_plugin_sdk::PromptFormField::new(field.key.clone(), field.label.clone(), kind)
}

fn json_to_toml_value(value: &serde_json::Value) -> toml::Value {
    match value {
        serde_json::Value::Bool(value) => toml::Value::Boolean(*value),
        serde_json::Value::Number(value) => value.as_i64().map_or_else(
            || toml::Value::String(value.to_string()),
            toml::Value::Integer,
        ),
        serde_json::Value::String(value) => toml::Value::String(value.clone()),
        other => toml::Value::String(other.to_string()),
    }
}

fn toml_to_json_value(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::Boolean(value) => serde_json::Value::Bool(*value),
        toml::Value::Integer(value) => serde_json::Value::Number((*value).into()),
        toml::Value::Float(value) => serde_json::Number::from_f64(*value).map_or_else(
            || serde_json::Value::String(value.to_string()),
            serde_json::Value::Number,
        ),
        toml::Value::String(value) => serde_json::Value::String(value.clone()),
        toml::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(toml_to_json_value).collect())
        }
        toml::Value::Table(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), toml_to_json_value(value)))
                .collect(),
        ),
        toml::Value::Datetime(value) => serde_json::Value::String(value.to_string()),
    }
}

fn toml_value_as_bool(value: &toml::Value) -> Option<bool> {
    match value {
        toml::Value::Boolean(value) => Some(*value),
        toml::Value::String(value) if value.eq_ignore_ascii_case("true") => Some(true),
        toml::Value::String(value) if value.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

fn builtin_theme_settings_payload_from_form(
    form: &ThemeSettingsFormSpec,
    values: &BTreeMap<String, bmux_plugin_sdk::PromptFormValue>,
) -> ThemeSettingsPayload {
    let mut json = serde_json::Map::new();
    for field in &form.fields {
        if let Some(value) = values.get(&field.key) {
            json.insert(field.key.clone(), prompt_value_to_json(value));
        }
    }
    ThemeSettingsPayload::from_value(&serde_json::Value::Object(json))
        .unwrap_or_else(|| ThemeSettingsPayload { json: Vec::new() })
}

fn prompt_value_to_json(value: &bmux_plugin_sdk::PromptFormValue) -> serde_json::Value {
    match value {
        bmux_plugin_sdk::PromptFormValue::Bool(value) => serde_json::Value::Bool(*value),
        bmux_plugin_sdk::PromptFormValue::Text(value)
        | bmux_plugin_sdk::PromptFormValue::Number(value)
        | bmux_plugin_sdk::PromptFormValue::Single(value) => {
            serde_json::Value::String(value.clone())
        }
        bmux_plugin_sdk::PromptFormValue::Integer(value) => {
            serde_json::Value::Number((*value).into())
        }
        bmux_plugin_sdk::PromptFormValue::Multi(values) => serde_json::Value::Array(
            values
                .iter()
                .cloned()
                .map(serde_json::Value::String)
                .collect(),
        ),
    }
}

fn json_settings_to_toml(value: &serde_json::Value) -> toml::Value {
    match value {
        serde_json::Value::Object(map) => toml::Value::Table(
            map.iter()
                .map(|(key, value)| (key.clone(), json_to_toml_value(value)))
                .collect(),
        ),
        value => json_to_toml_value(value),
    }
}

#[cfg(test)]
fn apply_builtin_component_theme_settings(
    context: &(impl ServiceCaller + Sync),
    theme: &ResolvedTheme,
    provider_id: &str,
    payload: &ThemeSettingsPayload,
    all_plugin_ids: &[String],
) {
    let settings_value = json_settings_to_toml(&payload_to_json_value(payload));
    let mut theme = theme.clone();
    apply_theme_settings_component_overrides(
        &mut theme,
        &BTreeMap::from([(provider_id.to_string(), settings_value)]),
    );
    apply_theme_extensions(context, &theme, all_plugin_ids, &[]);
}

fn persist_theme_settings(
    context: &impl ServiceCaller,
    provider_id: &str,
    provider: &ThemeSettingsProviderSpec,
    settings: &ThemeSettingsPayload,
) {
    let key = provider_storage_key(provider_id, provider);
    let request = StorageSetRequest::new(storage_key_from_string(&key), settings.json.clone());
    if let Err(error) = context.storage_set(&request) {
        warn!(%error, key, provider_id, "failed persisting theme settings");
    } else {
        info!(key, provider_id, "persisted theme settings");
    }
}

fn provider_storage_key(provider_id: &str, provider: &ThemeSettingsProviderSpec) -> String {
    provider
        .storage_key
        .clone()
        .unwrap_or_else(|| format!("theme_settings.{provider_id}"))
}

fn storage_key_from_string(key: &str) -> StorageKey {
    StorageKey::new(key).unwrap_or_else(|_| bmux_plugin_sdk::storage_key!("theme_settings.invalid"))
}

#[allow(clippy::result_large_err)] // Mirrors ServiceCaller's public result type for thin forwarding.
fn call_theme_settings_service<Request, Response>(
    context: &impl ServiceCaller,
    endpoint: &ThemeSettingsEndpoint,
    request: &Request,
) -> bmux_plugin_sdk::Result<Response>
where
    Request: Serialize,
    Response: serde::de::DeserializeOwned,
{
    context.call_service(
        &endpoint.capability,
        endpoint.kind.into(),
        &endpoint.interface_id,
        &endpoint.operation,
        request,
    )
}

fn try_parse_settings(settings: Option<&toml::Value>) -> Result<ThemePluginSettings, String> {
    settings
        .cloned()
        .map(toml::Value::try_into)
        .transpose()
        .map_err(|error| format!("invalid theme settings: {error}"))
        .map(Option::unwrap_or_default)
}

fn scoped_theme_settings_for_target(
    context: &(impl ThemeHostContext + ?Sized),
    target: ConfigScopeTarget,
) -> Result<ThemePluginSettings, String> {
    let connection = context.connection_info();
    let path = connection
        .probe_config_file("bmux.toml")
        .unwrap_or_else(|| config_paths_from_connection(connection).config_file());
    let request = ScopedConfigLoadRequest::new(target);
    let config = BmuxConfig::load_from_path_for_scope_with_overrides(
        &path,
        &ConfigLoadOverrides::for_process(),
        &request,
    )
    .map_err(|error| error.to_string())?;
    try_parse_settings(config.plugins.settings.get("bmux.theme"))
}

fn config_paths_from_connection(connection: &HostConnectionInfo) -> bmux_config::ConfigPaths {
    bmux_config::ConfigPaths::new(
        Path::new(&connection.config_dir).to_path_buf(),
        Path::new(&connection.runtime_dir).to_path_buf(),
        Path::new(&connection.data_dir).to_path_buf(),
        Path::new(&connection.state_dir).to_path_buf(),
    )
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
) -> Option<ActiveThemeStack> {
    let persisted = if matches!(
        settings.persistence,
        ThemePersistence::PersistBetweenConnects
    ) {
        match read_persisted_theme_name(context) {
            Ok(selection) => selection,
            Err(error) => {
                warn!(%error, "theme selection requires recovery");
                return None;
            }
        }
    } else {
        None
    };
    if let Some(name) = persisted {
        if theme_by_name(catalog, &name).is_some() {
            info!(theme = %name, "using persisted theme selection");
            let stack = filter_existing_theme_names(catalog, base_theme_stack(&name));
            return Some(ActiveThemeStack {
                stack,
                source: ActiveThemeSource::Persisted,
                requested_name: Some(name),
            });
        }
        warn!(theme = %name, "persisted theme no longer exists in catalog; falling back to declared theme");
    }
    Some(active_stack_from_requested(
        catalog,
        declared_theme_stack(settings),
        if settings.themes.is_empty() {
            ActiveThemeSource::Declared
        } else {
            ActiveThemeSource::DeclaredStack
        },
        settings.theme.clone(),
    ))
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
    let mut theme_settings = ThemeSettingsConfig::default();
    for name in stack {
        let theme = theme_by_name(catalog, name)?;
        apply_theme_layer(&mut appearance, &mut plugins, &mut theme_settings, theme);
    }
    Some(ResolvedTheme {
        external_payloads: BTreeMap::new(),
        appearance,
        plugins,
        settings: theme_settings,
    })
}

fn resolve_theme_stack_with_settings(
    catalog: &[ThemeCatalogEntry],
    stack: &[String],
    settings: &ThemePluginSettings,
) -> Option<ResolvedTheme> {
    let mut theme = if settings.appearance_themes.is_empty() && settings.component_themes.is_empty()
    {
        resolve_theme_stack(catalog, stack)?
    } else {
        resolve_split_theme_stack(catalog, stack, settings)?
    };
    apply_theme_settings_component_overrides(&mut theme, &settings.theme_settings);
    apply_settings_component_overrides(&mut theme, &settings.components);
    apply_settings_component_target_overrides(&mut theme, &settings.component_targets);
    Some(theme)
}

fn validate_provider_transition(
    previous: &ResolvedTheme,
    next: &ResolvedTheme,
) -> Result<(), String> {
    for id in previous.external_payloads.keys() {
        if !next.external_payloads.contains_key(id)
            && previous
                .settings
                .providers
                .get(id)
                .and_then(|provider| provider.reset_settings.as_ref())
                .is_none()
        {
            return Err(format!(
                "settings provider {id} requires an explicit reset contract before removing its active payload"
            ));
        }
    }
    Ok(())
}

fn reset_removed_providers(
    context: &impl ServiceCaller,
    previous: &ResolvedTheme,
    next: &ResolvedTheme,
) -> Result<(), String> {
    validate_provider_transition(previous, next)?;
    for id in previous.external_payloads.keys() {
        if next.external_payloads.contains_key(id) {
            continue;
        }
        let endpoint = previous
            .settings
            .providers
            .get(id)
            .and_then(|provider| provider.reset_settings.as_ref())
            .ok_or("missing reset contract")?;
        call_theme_settings_service::<_, ()>(context, endpoint, &())
            .map_err(|error| format!("reset provider {id}: {error}"))?;
    }
    Ok(())
}

fn resolve_live_selection(
    context: &(impl ServiceCaller + ?Sized),
    catalog: &[ThemeCatalogEntry],
    selection: &ThemeSelection,
    settings: &ThemePluginSettings,
) -> Result<ResolvedTheme, String> {
    let mut theme = resolve_picker_value(catalog, &selection.picker_value(), settings)
        .ok_or("theme not found")?;
    let mut overrides = BTreeMap::new();
    for (id, provider) in &theme.settings.providers {
        let stored = if matches!(selection, ThemeSelection::Preset { .. }) {
            let key = StorageKey::new(provider_storage_key(id, provider))
                .map_err(|error| error.to_string())?;
            context
                .storage_get(&StorageGetRequest::new(key))
                .map_err(|error| error.to_string())?
                .value
        } else {
            None
        };
        let value = if let Some(bytes) = stored {
            if bytes.len() > 65_536 {
                return Err("stored theme settings exceed 64 KiB".into());
            }
            serde_json::from_slice::<serde_json::Value>(&bytes)
                .map_err(|error| format!("invalid stored settings for {id}: {error}"))?
        } else if let Some(value) = settings.theme_settings.get(id) {
            serde_json::to_value(value).map_err(|error| error.to_string())?
        } else {
            continue;
        };
        if !value.is_object() {
            return Err(format!("settings for {id} must be an object"));
        }
        if theme.settings.component_settings.contains_key(id) {
            overrides.insert(id.clone(), json_settings_to_toml(&value));
        } else {
            let payload =
                ThemeSettingsPayload::from_value(&value).ok_or("invalid provider payload")?;
            theme.external_payloads.insert(id.clone(), payload);
        }
    }
    apply_theme_settings_component_overrides(&mut theme, &overrides);
    Ok(theme)
}

fn resolve_picker_value(
    catalog: &[ThemeCatalogEntry],
    value: &str,
    settings: &ThemePluginSettings,
) -> Option<ResolvedTheme> {
    match ThemeSelection::from_picker_value(value)? {
        ThemeSelection::Configured => {
            resolve_theme_stack_with_settings(catalog, &declared_theme_stack(settings), settings)
        }
        ThemeSelection::Preset { name } => resolve_theme_picker_selection(catalog, &name, settings),
    }
}

fn resolve_theme_picker_selection(
    catalog: &[ThemeCatalogEntry],
    name: &str,
    settings: &ThemePluginSettings,
) -> Option<ResolvedTheme> {
    let mut theme = resolve_theme_stack(catalog, &base_theme_stack(name))?;
    apply_theme_settings_component_overrides(&mut theme, &settings.theme_settings);
    apply_settings_component_overrides(&mut theme, &settings.components);
    apply_settings_component_target_overrides(&mut theme, &settings.component_targets);
    Some(theme)
}

fn resolve_split_theme_stack(
    catalog: &[ThemeCatalogEntry],
    active_stack: &[String],
    settings: &ThemePluginSettings,
) -> Option<ResolvedTheme> {
    let appearance_stack = if settings.appearance_themes.is_empty() {
        active_stack.to_vec()
    } else {
        normalize_theme_name_list(&settings.appearance_themes)
    };
    let component_stack = if settings.component_themes.is_empty() {
        active_stack.to_vec()
    } else {
        normalize_theme_name_list(&settings.component_themes)
    };
    if appearance_stack.is_empty() && component_stack.is_empty() {
        return None;
    }
    let mut appearance = RuntimeAppearance::default();
    let mut plugins = BTreeMap::new();
    let mut theme_settings = ThemeSettingsConfig::default();
    for name in filter_existing_theme_names(catalog, appearance_stack) {
        let theme = theme_by_name(catalog, &name)?;
        apply_theme_appearance_layer(&mut appearance, theme);
        apply_theme_plugin_layer(&mut plugins, &appearance_only_plugin_extensions(theme));
        apply_theme_settings_layer(&mut theme_settings, &theme.settings);
    }
    for name in filter_existing_theme_names(catalog, component_stack) {
        let theme = theme_by_name(catalog, &name)?;
        apply_theme_component_layer(&mut plugins, &mut theme_settings, theme);
    }
    Some(ResolvedTheme {
        external_payloads: BTreeMap::new(),
        appearance,
        plugins,
        settings: theme_settings,
    })
}

fn normalize_theme_name_list(names: &[String]) -> Vec<String> {
    names
        .iter()
        .map(|name| normalized_theme_name(name))
        .collect()
}

fn apply_settings_component_overrides(
    theme: &mut ResolvedTheme,
    components: &BTreeMap<String, toml::Value>,
) {
    if components.is_empty() {
        return;
    }
    merge_decoration_component_overrides(theme, components);
}

fn apply_settings_component_target_overrides(
    theme: &mut ResolvedTheme,
    targets: &BTreeMap<String, toml::Value>,
) {
    if targets.is_empty() {
        return;
    }
    let Some(component_ids) = decoration_component_ids(theme) else {
        return;
    };
    let mut components = BTreeMap::new();
    for component_id in component_ids {
        for (pattern, target) in targets {
            if component_target_pattern_matches(pattern, &component_id) {
                components.insert(
                    component_id.clone(),
                    toml::Value::Table(toml::map::Map::from_iter([(
                        "target".to_string(),
                        target.clone(),
                    )])),
                );
            }
        }
    }
    merge_decoration_component_overrides(theme, &components);
}

fn decoration_component_ids(theme: &ResolvedTheme) -> Option<Vec<String>> {
    let components = theme
        .plugins
        .get("bmux.decoration")?
        .as_table()?
        .get("components")?
        .as_table()?;
    Some(components.keys().cloned().collect())
}

fn component_target_pattern_matches(pattern: &str, component_id: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(".*") {
        return component_id
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('.'));
    }
    pattern == component_id
}

fn apply_theme_settings_component_overrides(
    theme: &mut ResolvedTheme,
    theme_settings: &BTreeMap<String, toml::Value>,
) {
    if theme_settings.is_empty() {
        return;
    }
    let mut components = BTreeMap::new();
    for (settings_id, value) in theme_settings {
        let Some(spec) = theme.settings.component_settings.get(settings_id) else {
            continue;
        };
        let Some(settings_table) = component_settings_table(value) else {
            continue;
        };
        for component_id in &spec.components {
            components.insert(
                component_id.clone(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "settings".to_string(),
                    toml::Value::Table(settings_table.clone()),
                )])),
            );
        }
    }
    merge_decoration_component_overrides(theme, &components);
}

fn component_settings_table(value: &toml::Value) -> Option<toml::map::Map<String, toml::Value>> {
    let table = value.as_table()?;
    Some(
        table
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    toml::Value::String(component_setting_string(value)),
                )
            })
            .collect(),
    )
}

fn component_setting_string(value: &toml::Value) -> String {
    match value {
        toml::Value::String(value) => value.clone(),
        toml::Value::Integer(value) => value.to_string(),
        toml::Value::Float(value) => value.to_string(),
        toml::Value::Boolean(value) => value.to_string(),
        other => other.to_string(),
    }
}

fn merge_decoration_component_overrides(
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
    settings: &mut ThemeSettingsConfig,
    theme: &ThemeConfig,
) {
    apply_theme_appearance_layer(appearance, theme);
    apply_theme_plugin_layer(plugins, &theme.plugins);
    apply_theme_settings_layer(settings, &theme.settings);
}

fn apply_theme_appearance_layer(appearance: &mut RuntimeAppearance, theme: &ThemeConfig) {
    appearance.apply_patch(&RuntimeAppearancePatch::from(theme));
    for (mode_id, mode_theme) in &theme.modes {
        let patch = appearance
            .modes
            .entry(normalized_theme_name(mode_id))
            .or_default();
        patch.merge(&RuntimeAppearancePatch::from(mode_theme));
    }
}

fn apply_theme_component_layer(
    plugins: &mut BTreeMap<String, toml::Value>,
    settings: &mut ThemeSettingsConfig,
    theme: &ThemeConfig,
) {
    let component_plugins = theme
        .plugins
        .iter()
        .map(|(plugin_id, extension)| {
            (
                plugin_id.clone(),
                component_only_plugin_extension(plugin_id, extension),
            )
        })
        .filter(|(_, extension)| !extension_is_empty(extension))
        .collect::<BTreeMap<_, _>>();
    apply_theme_plugin_layer(plugins, &component_plugins);
    apply_theme_settings_layer(settings, &theme.settings);
}

fn apply_theme_plugin_layer(
    plugins: &mut BTreeMap<String, toml::Value>,
    plugin_extensions: &BTreeMap<String, toml::Value>,
) {
    for (plugin_id, extension) in plugin_extensions {
        match plugins.get_mut(plugin_id) {
            Some(existing) => merge_toml_value(existing, extension),
            None => {
                plugins.insert(plugin_id.clone(), extension.clone());
            }
        }
    }
}

fn appearance_only_plugin_extensions(theme: &ThemeConfig) -> BTreeMap<String, toml::Value> {
    theme
        .plugins
        .iter()
        .map(|(plugin_id, extension)| {
            (
                plugin_id.clone(),
                appearance_only_plugin_extension(plugin_id, extension),
            )
        })
        .filter(|(_, extension)| !extension_is_empty(extension))
        .collect()
}

fn appearance_only_plugin_extension(plugin_id: &str, extension: &toml::Value) -> toml::Value {
    if plugin_id != "bmux.decoration" {
        return extension.clone();
    }
    let Some(table) = extension.as_table() else {
        return extension.clone();
    };
    let mut filtered = table.clone();
    for key in [
        "animation",
        "components",
        "input",
        "script",
        "script_access",
    ] {
        filtered.remove(key);
    }
    toml::Value::Table(filtered)
}

fn component_only_plugin_extension(plugin_id: &str, extension: &toml::Value) -> toml::Value {
    if plugin_id != "bmux.decoration" {
        return extension.clone();
    }
    let Some(table) = extension.as_table() else {
        return extension.clone();
    };
    let has_components = table
        .get("components")
        .and_then(toml::Value::as_table)
        .is_some_and(|components| !components.is_empty());
    let mut filtered = toml::map::Map::new();
    for key in [
        "components",
        "script",
        "script_access",
        "animation",
        "input",
    ] {
        if has_components && key == "script" {
            continue;
        }
        if let Some(value) = table.get(key) {
            filtered.insert(key.to_string(), value.clone());
        }
    }
    toml::Value::Table(filtered)
}

fn extension_is_empty(extension: &toml::Value) -> bool {
    extension.as_table().is_some_and(toml::map::Map::is_empty)
}

fn apply_theme_settings_layer(
    settings: &mut ThemeSettingsConfig,
    theme_settings: &ThemeSettingsConfig,
) {
    for (provider_id, provider) in &theme_settings.providers {
        settings
            .providers
            .insert(provider_id.clone(), provider.clone());
    }
    for (settings_id, component_settings) in &theme_settings.component_settings {
        settings
            .component_settings
            .insert(settings_id.clone(), component_settings.clone());
    }
    for (form_id, form) in &theme_settings.forms {
        settings.forms.insert(form_id.clone(), form.clone());
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

fn read_persisted_theme_name(
    context: &(impl ThemeHostContext + ?Sized),
) -> Result<Option<String>, String> {
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
            return Err(error.to_string());
        }
    };
    let Some(value) = response.value else {
        info!(
            data_dir = %context.connection_info().data_dir,
            key = STORAGE_SELECTED_APPEARANCE,
            "no persisted theme selection found",
        );
        return Ok(None);
    };
    decode_theme_selection(&value)
}

fn persist_theme_name(context: &impl ThemeHostContext, name: &str) -> Result<(), String> {
    let selection = ThemeSelection::from_picker_value(name).ok_or("invalid picker selection")?;
    let result = context.storage_set(&StorageSetRequest::new(
        bmux_plugin_sdk::storage_key!("selected_theme"),
        serde_json::to_vec(&PersistedThemeSelection {
            version: 1,
            preset: match selection {
                ThemeSelection::Configured => None,
                ThemeSelection::Preset { name } => Some(name),
            },
        })
        .expect("theme selection contains only JSON-serializable fields"),
    ));
    if let Err(error) = result {
        warn!(
            %error,
            data_dir = %context.connection_info().data_dir,
            key = STORAGE_SELECTED_APPEARANCE,
            theme = %name,
            "failed persisting selected theme",
        );
        Err(error.to_string())
    } else {
        info!(
            data_dir = %context.connection_info().data_dir,
            key = STORAGE_SELECTED_APPEARANCE,
            theme = %name,
            "persisted selected theme",
        );
        Ok(())
    }
}

fn load_theme_catalog(config_dir_candidates: &[std::path::PathBuf]) -> Vec<ThemeCatalogEntry> {
    let mut entries = vec![ThemeCatalogEntry {
        name: "default".to_string(),
        theme: ThemeConfig::default(),
    }];

    for (name, text) in bundled_theme_presets() {
        if let Ok(theme) = toml::from_str::<ThemeConfig>(text) {
            upsert_theme_catalog_entry(&mut entries, (*name).to_string(), theme);
        }
    }

    // Candidate directories are highest priority first. Apply them in reverse
    // so the first candidate overrides fallback directories and bundled defaults.
    for dir in config_dir_candidates.iter().rev() {
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

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

fn load_theme_file(path: &Path, entries: &mut Vec<ThemeCatalogEntry>) {
    let Some(name) = path.file_stem().and_then(std::ffi::OsStr::to_str) else {
        return;
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            warn!(path = %path.display(), %error, "failed reading theme file");
            entries.retain(|entry| entry.name != name);
            return;
        }
    };
    match toml::from_str::<ThemeConfig>(&text) {
        Ok(theme) => upsert_theme_catalog_entry(entries, name.to_string(), theme),
        Err(error) => {
            warn!(path = %path.display(), %error, "invalid theme file");
            // Do not silently substitute a lower-priority preset with the same name.
            entries.retain(|entry| entry.name != name);
        }
    }
}

const fn bundled_theme_presets() -> &'static [(&'static str, &'static str)] {
    &[
        ("hacker", include_str!("../assets/themes/hacker.toml")),
        ("cyberpunk", include_str!("../assets/themes/cyberpunk.toml")),
        ("minimal", include_str!("../assets/themes/minimal.toml")),
        (
            "pulse-border",
            include_str!("../assets/themes/pulse-border.toml"),
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
        ("tetris", include_str!("../assets/themes/tetris.toml")),
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

fn picker_options(
    catalog: &[ThemeCatalogEntry],
    settings: &ThemePluginSettings,
    active_name: &str,
) -> Vec<bmux_plugin_sdk::PromptOption> {
    let fallback = declared_theme_stack(settings);
    let appearance = if settings.appearance_themes.is_empty() {
        &fallback
    } else {
        &settings.appearance_themes
    };
    let components = if settings.component_themes.is_empty() {
        &fallback
    } else {
        &settings.component_themes
    };
    let label = format!(
        "Use configured theme{} — appearance: {}; components: {}",
        if active_name == CONFIGURED_SELECTION {
            " (active)"
        } else {
            ""
        },
        appearance.join(" → "),
        components.join(" → ")
    );
    let mut options = vec![bmux_plugin_sdk::PromptOption::new(
        CONFIGURED_SELECTION,
        label,
    )];
    options.extend(prompt_options(catalog, active_name));
    options
}

fn prompt_options(
    catalog: &[ThemeCatalogEntry],
    active_name: &str,
) -> Vec<bmux_plugin_sdk::PromptOption> {
    let preset = active_name.strip_prefix("preset:").unwrap_or(active_name);
    let active_name = theme_by_name(catalog, preset).map_or(preset, |theme| &theme.name);
    catalog
        .iter()
        .map(|entry| {
            let mut label = entry.name.clone();
            if entry.name == active_name {
                label.push_str(" (active)");
            }
            bmux_plugin_sdk::PromptOption::new(
                ThemeSelection::Preset {
                    name: entry.name.clone(),
                }
                .picker_value(),
                label,
            )
        })
        .collect()
}

fn selected_index(catalog: &[ThemeCatalogEntry], active_name: &str) -> usize {
    if active_name == CONFIGURED_SELECTION {
        return 0;
    }
    let active_name = active_name.strip_prefix("preset:").unwrap_or(active_name);
    let active_name = theme_by_name(catalog, active_name).map_or(active_name, |theme| &theme.name);
    catalog
        .iter()
        .position(|entry| entry.name == active_name)
        .map_or(0, |index| index + 1)
}

fn theme_by_name<'a>(catalog: &'a [ThemeCatalogEntry], name: &str) -> Option<&'a ThemeConfig> {
    // Preserve explicitly named user themes; the deprecated bundled name is
    // only a lookup fallback, never a second catalog/picker entry.
    catalog
        .iter()
        .find(|entry| entry.name == name)
        .or_else(|| {
            (name == "pulse-demo")
                .then(|| catalog.iter().find(|entry| entry.name == "pulse-border"))
                .flatten()
        })
        .map(|entry| &entry.theme)
}

#[cfg(test)]
fn apply_theme_extensions(
    context: &(impl ServiceCaller + Sync),
    theme: &ResolvedTheme,
    plugin_ids: &[String],
    config_dir_candidates: &[String],
) {
    if let Err(error) =
        try_apply_theme_extensions(context, theme, plugin_ids, config_dir_candidates)
    {
        warn!(%error, "theme extension application failed");
    }
}

fn try_apply_theme_extensions(
    context: &(impl ServiceCaller + Sync),
    theme: &ResolvedTheme,
    plugin_ids: &[String],
    config_dir_candidates: &[String],
) -> Result<(), String> {
    for plugin_id in plugin_ids {
        let toml = match theme.plugins.get(plugin_id) {
            Some(extension) => {
                toml::to_string(extension).map_err(|error| format!("{plugin_id}: {error}"))?
            }
            None => String::new(),
        };
        let request = ApplyThemeExtensionArgs {
            toml,
            config_dir_candidates: config_dir_candidates.to_vec(),
        };
        let has_extension = !request.toml.trim().is_empty();
        let payload = bmux_plugin_sdk::encode_service_message(&request)
            .map_err(|error| format!("{plugin_id}: {error}"))?;
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
            return Err(format!("{plugin_id}: {error}"));
        }
        info!(
            plugin_id = %plugin_id,
            capability = %capability,
            has_extension,
            "theme extension apply completed",
        );
    }
    for (id, payload) in &theme.external_payloads {
        let provider = theme
            .settings
            .providers
            .get(id)
            .ok_or_else(|| format!("missing settings provider {id}"))?;
        apply_theme_settings_provider_payload(context, id, provider, payload)?;
    }
    Ok(())
}

fn execute_theme_extension_apply(
    context: &(impl ServiceCaller + Sync),
    capability: &str,
    payload: Vec<u8>,
) -> std::result::Result<(), String> {
    if capability == "bmux.decoration.write" {
        let request: ApplyThemeExtensionArgs =
            bmux_plugin_sdk::decode_service_message(&payload).map_err(|error| error.to_string())?;
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
        return bmux_plugin::block_on_typed_dispatch(
            bmux_decoration_plugin_api::decoration_commands::client::apply_theme_extension(
                &mut client,
                request.toml,
                request.config_dir_candidates,
            ),
        )
        .map_err(|error| error.to_string())?
        .map_err(|error| format!("decoration rejected theme: {error:?}"));
    }
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
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_theme_dir(label: &str) -> std::path::PathBuf {
        let unique = format!(
            "bmux-theme-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before epoch")
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).expect("create temp theme dir");
        dir
    }

    #[test]
    fn picker_identity_does_not_collide_with_theme_names() {
        for name in ["configured:", "preset:hacker", "hacker"] {
            let selection = ThemeSelection::Preset {
                name: name.to_string(),
            };
            assert_eq!(
                ThemeSelection::from_picker_value(&selection.picker_value()),
                Some(selection)
            );
        }
        assert_eq!(
            ThemeSelection::from_picker_value(CONFIGURED_SELECTION),
            Some(ThemeSelection::Configured)
        );
        assert!(ThemeSelection::from_picker_value("unqualified").is_none());
        assert!(ThemeSelection::from_picker_value("preset:").is_none());
        assert!(ThemeSelection::from_picker_value("preset:  ").is_none());
    }

    #[test]
    fn selection_records_decode_legacy_configured_and_reject_unknown_versions() {
        assert_eq!(
            decode_theme_selection(b"hacker").expect("legacy"),
            Some("hacker".into())
        );
        assert_eq!(
            decode_theme_selection(br#"{"version":1,"preset":null}"#).expect("configured"),
            None
        );
        assert!(decode_theme_selection(br#"{"version":2,"preset":null}"#).is_err());
        assert!(decode_theme_selection(br#"{"version":1}"#).is_err());
        assert!(decode_theme_selection(br#"{"version":1,"preset":" "}"#).is_err());
        assert!(decode_theme_selection(br#"{"version":1,"preset":null,"extra":true}"#).is_err());
        assert!(decode_theme_selection(br#"{"version":1,"version":1,"preset":null}"#).is_err());
        assert!(decode_theme_selection(b"{broken").is_err());
        assert!(decode_theme_selection(b"").is_err());
    }

    #[test]
    fn configured_picker_resolves_full_split_composition() {
        let settings: ThemePluginSettings = toml::from_str(
            r#"
            appearance_themes = ["performance", "mode-aware"]
            component_themes = ["performance", "pulse-border"]
        "#,
        )
        .expect("settings");
        let catalog = load_theme_catalog(&[]);
        let configured =
            resolve_picker_value(&catalog, CONFIGURED_SELECTION, &settings).expect("configured");
        let expected = resolve_theme_stack_with_settings(
            &catalog,
            &declared_theme_stack(&settings),
            &settings,
        )
        .expect("expected");
        assert_eq!(configured.plugins, expected.plugins);
        assert_eq!(configured.appearance, expected.appearance);
        let options = picker_options(&catalog, &settings, CONFIGURED_SELECTION);
        assert_eq!(options.len(), catalog.len() + 1);
        assert_eq!(selected_index(&catalog, CONFIGURED_SELECTION), 0);
    }

    fn picker_context() -> NativeCommandContext {
        let lifecycle = lifecycle_context(None);
        NativeCommandContext {
            plugin_id: lifecycle.plugin_id,
            command: "pick-theme".to_string(),
            arguments: Vec::new(),
            required_capabilities: lifecycle.required_capabilities,
            provided_capabilities: lifecycle.provided_capabilities,
            services: lifecycle.services,
            available_capabilities: lifecycle.available_capabilities,
            enabled_plugins: lifecycle.enabled_plugins,
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            active_keybindings: Vec::new(),
            host: lifecycle.host,
            connection: lifecycle.connection,
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            caller_client_id: None,
            invocation_source: bmux_plugin_sdk::NativeCommandInvocationSource::Unknown,
            host_kernel_bridge: None,
        }
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    async fn picker_rejection_and_closed_events_do_not_apply_themes() {
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let router: TestServiceRouter = Arc::new(move |_, _, capability, _, _, _, _| {
            if capability == "bmux.theme.write" {
                return encode_service_message(&control_contract::theme_control_v1::Snapshot {
                    revision: 0,
                    selection: ThemeSelection::Configured,
                });
            }
            *observed.lock().expect("calls") += 1;
            Ok(Vec::new())
        });
        let _router = install_test_service_router(router);
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let _host = prompt::register_host(sender);
        let host = async {
            let request = requests.recv().await.expect("prompt");
            drop(request.event_tx);
            // Yield with a closed event stream before delivering the response.
            tokio::task::yield_now().await;
            request
                .response_tx
                .send(PromptResponse::RejectedBusy)
                .expect("response");
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(run_theme_picker(picker_context()), host);
        })
        .await
        .expect("picker must finish without spinning");
        assert_eq!(*calls.lock().expect("calls"), 0);
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    async fn picker_delegates_failed_selection_to_control_authority() {
        let applied = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed = applied.clone();
        let router: TestServiceRouter =
            Arc::new(move |_, _, capability, _, _, operation, payload| {
                match (capability, operation) {
                    ("bmux.theme.write", "current") => {
                        encode_service_message(&control_contract::theme_control_v1::Snapshot {
                            revision: 0,
                            selection: ThemeSelection::Preset {
                                name: "hacker".into(),
                            },
                        })
                    }
                    ("bmux.theme.write", "select") => {
                        let request: control_contract::theme_control_v1::client::SelectRequest =
                            decode_service_message(&payload).expect("selection request");
                        assert_eq!(request.expected_revision, 0);
                        assert_eq!(
                            request.selection,
                            ThemeSelection::Preset {
                                name: "minimal".into()
                            }
                        );
                        observed.lock().expect("calls").push("select".into());
                        encode_service_message(&Err::<
                            control_contract::theme_control_v1::Snapshot,
                            String,
                        >("storage failure".into()))
                    }
                    ("bmux.storage", "get") => encode_service_message(&StorageGetResponse {
                        value: Some(b"hacker".to_vec()),
                    }),
                    ("bmux.storage", "set") => Err(bmux_plugin_sdk::PluginError::InvalidPluginId {
                        id: "injected storage failure".into(),
                    }),
                    ("bmux.decoration.write", "apply-theme-extension") => {
                        let request: ApplyThemeExtensionArgs =
                            decode_service_message(&payload).expect("extension");
                        observed.lock().expect("applied").push(request.toml);
                        encode_service_message(&Ok::<
                            (),
                            bmux_decoration_plugin_api::decoration_state::ValidationResult,
                        >(()))
                    }
                    _ => encode_service_message(&()),
                }
            });
        let _router = install_test_service_router(router);
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let _host = prompt::register_host(sender);
        let mut context = picker_context();
        context.settings =
            Some(toml::from_str("persistence = 'persist_between_connects'").expect("settings"));
        let host = async {
            let request = requests.recv().await.expect("prompt");
            request
                .response_tx
                .send(PromptResponse::Submitted(PromptValue::Single(
                    "preset:minimal".into(),
                )))
                .expect("response");
        };
        tokio::join!(run_theme_picker(context), host);
        assert_eq!(&*applied.lock().expect("calls"), &["select"]);
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn typed_decoration_apply_surfaces_validation_failure() {
        let router: TestServiceRouter = Arc::new(|_, _, capability, _, interface, operation, _| {
            assert_eq!(capability, "bmux.decoration.write");
            assert_eq!(interface, "decoration-commands");
            assert_eq!(operation, "apply-theme-extension");
            encode_service_message(&Err::<(), _>(
                bmux_decoration_plugin_api::decoration_state::ValidationResult::Errors {
                    errors: vec![
                        bmux_decoration_plugin_api::decoration_state::ValidationError {
                            path: "script".into(),
                            message: "invalid Lua".into(),
                        },
                    ],
                },
            ))
        });
        let _router = install_test_service_router(router);
        let payload = encode_service_message(&ApplyThemeExtensionArgs {
            toml: String::new(),
            config_dir_candidates: Vec::new(),
        })
        .expect("encode");
        let error =
            execute_theme_extension_apply(&picker_context(), "bmux.decoration.write", payload)
                .expect_err("validation must propagate");
        assert!(error.contains("invalid Lua"));
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    async fn rejected_decoration_is_not_persisted_by_picker() {
        let writes = Arc::new(Mutex::new(0));
        let observed = writes.clone();
        let router: TestServiceRouter = Arc::new(
            move |_, _, capability, _, _, operation, _| match (capability, operation) {
                ("bmux.theme.write", "current") => {
                    encode_service_message(&control_contract::theme_control_v1::Snapshot {
                        revision: 0,
                        selection: ThemeSelection::Preset {
                            name: "hacker".into(),
                        },
                    })
                }
                ("bmux.storage", "get") => encode_service_message(&StorageGetResponse {
                    value: Some(b"hacker".to_vec()),
                }),
                ("bmux.storage", "set") => {
                    *observed.lock().expect("writes") += 1;
                    encode_service_message(&())
                }
                ("bmux.decoration.write", "apply-theme-extension") => {
                    encode_service_message(&Err::<(), _>(
                        bmux_decoration_plugin_api::decoration_state::ValidationResult::Errors {
                            errors: Vec::new(),
                        },
                    ))
                }
                _ => encode_service_message(&()),
            },
        );
        let _router = install_test_service_router(router);
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let _host = prompt::register_host(sender);
        let mut context = picker_context();
        context.settings =
            Some(toml::from_str("persistence = 'persist_between_connects'").expect("settings"));
        let host = async {
            let request = requests.recv().await.expect("prompt");
            request
                .response_tx
                .send(PromptResponse::Submitted(PromptValue::Single(
                    "preset:minimal".into(),
                )))
                .expect("response");
        };
        tokio::join!(run_theme_picker(context), host);
        assert_eq!(*writes.lock().expect("writes"), 0);
    }

    #[test]
    fn catalog_user_files_override_bundled_and_fallback_presets() {
        let primary = temp_theme_dir("catalog-primary");
        let fallback = temp_theme_dir("catalog-fallback");
        for (dir, color) in [(&primary, "#112233"), (&fallback, "#445566")] {
            std::fs::create_dir_all(dir.join("themes")).expect("themes directory");
            std::fs::write(
                dir.join("themes/hacker.toml"),
                format!("foreground = '{color}'"),
            )
            .expect("theme");
        }
        let catalog = load_theme_catalog(&[primary.clone(), fallback]);
        assert_eq!(
            theme_by_name(&catalog, "hacker")
                .expect("theme")
                .foreground
                .as_deref(),
            Some("#112233")
        );
        assert_eq!(
            catalog
                .iter()
                .filter(|entry| entry.name == "hacker")
                .count(),
            1
        );
        std::fs::write(primary.join("themes/hacker.toml"), "invalid = [").expect("invalid theme");
        let catalog = load_theme_catalog(&[primary]);
        assert!(theme_by_name(&catalog, "hacker").is_none());
        assert!(theme_by_name(&catalog, "minimal").is_some());
    }

    #[test]
    fn active_appearance_query_returns_published_live_appearance() {
        let catalog = load_theme_catalog(&[]);
        let live =
            resolve_theme_picker_selection(&catalog, "hacker", &ThemePluginSettings::default())
                .expect("live theme");
        publish_runtime_appearance(&live);
        let context = service_context(Some(
            toml::from_str("theme = 'minimal'").expect("startup settings"),
        ));
        let actual = active_runtime_appearance(&context).expect("retained appearance");
        assert_eq!(actual, live.appearance);
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn startup_rejection_keeps_previous_published_appearance() {
        let previous = resolve_theme_picker_selection(
            &load_theme_catalog(&[]),
            "hacker",
            &ThemePluginSettings::default(),
        )
        .expect("previous");
        publish_runtime_appearance(&previous);
        let router: TestServiceRouter = Arc::new(|_, _, _, _, _, _, _| {
            encode_service_message(&Err::<(), _>(
                bmux_decoration_plugin_api::decoration_state::ValidationResult::Errors {
                    errors: Vec::new(),
                },
            ))
        });
        let _router = install_test_service_router(router);
        let context =
            lifecycle_context(Some(toml::from_str("theme = 'minimal'").expect("settings")));
        ThemePlugin::default()
            .activate(context.clone())
            .expect("activation remains available");
        assert_eq!(
            active_runtime_appearance(&context).expect("retained"),
            previous.appearance
        );
    }

    #[test]
    fn control_current_retains_selection_across_config_changes() {
        let plugin = ThemePlugin::default();
        let mut context =
            service_context(Some(toml::from_str("theme = 'hacker'").expect("settings")));
        context.request.service.interface_id = "theme-control-v1".to_string();
        context.request.operation = "current".to_string();
        let first = plugin.invoke_service(context.clone());
        assert!(first.error.is_none());
        context.settings = Some(toml::from_str("theme = 'minimal'").expect("settings"));
        let second = plugin.invoke_service(context);
        assert!(second.error.is_none());
        assert_eq!(first.payload, second.payload);
        let state: control_contract::theme_control_v1::Snapshot =
            decode_service_message(&second.payload).expect("snapshot");
        assert_eq!(state.revision, 0);
        assert_eq!(state.selection, ThemeSelection::Configured);
    }

    #[test]
    fn stale_selection_revision_is_rejected_before_effects() {
        let plugin = ThemePlugin {
            selection: std::sync::Mutex::new(Some(LiveTheme {
                snapshot: control_contract::theme_control_v1::Snapshot {
                    revision: 4,
                    selection: ThemeSelection::Configured,
                },
                resolved: resolve_picker_value(
                    &load_theme_catalog(&[]),
                    CONFIGURED_SELECTION,
                    &ThemePluginSettings::default(),
                )
                .expect("theme"),
            })),
            ..ThemePlugin::default()
        };
        let result = plugin.select_live_theme(
            &service_context(None),
            3,
            ThemeSelection::Preset {
                name: "hacker".into(),
            },
        );
        assert_eq!(result.expect_err("stale revision"), "stale theme revision");
        assert_eq!(
            plugin
                .selection
                .lock()
                .expect("state")
                .as_ref()
                .expect("snapshot")
                .snapshot
                .revision,
            4
        );
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn live_selection_failure_preserves_revision_and_resolved_theme() {
        let original = resolve_picker_value(
            &load_theme_catalog(&[]),
            CONFIGURED_SELECTION,
            &ThemePluginSettings::default(),
        )
        .expect("theme");
        let plugin = ThemePlugin {
            selection: std::sync::Mutex::new(Some(LiveTheme {
                snapshot: control_contract::theme_control_v1::Snapshot {
                    revision: 4,
                    selection: ThemeSelection::Configured,
                },
                resolved: original.clone(),
            })),
            ..ThemePlugin::default()
        };
        let router: TestServiceRouter = Arc::new(|_, _, capability, _, _, operation, _| {
            if capability == "bmux.storage" && operation == "set" {
                return Err(bmux_plugin_sdk::PluginError::InvalidPluginId {
                    id: "storage failure".into(),
                });
            }
            encode_service_message(&Ok::<
                (),
                bmux_decoration_plugin_api::decoration_state::ValidationResult,
            >(()))
        });
        let _router = install_test_service_router(router);
        let context = service_context(Some(
            toml::from_str("persistence = 'persist_between_connects'").expect("settings"),
        ));
        assert!(
            plugin
                .select_live_theme(
                    &context,
                    4,
                    ThemeSelection::Preset {
                        name: "hacker".into()
                    }
                )
                .is_err()
        );
        let live = plugin
            .selection
            .lock()
            .expect("state")
            .clone()
            .expect("live");
        assert_eq!(live.snapshot.revision, 4);
        assert_eq!(live.snapshot.selection, ThemeSelection::Configured);
        assert_eq!(live.resolved.appearance, original.appearance);
        assert_eq!(live.resolved.plugins, original.plugins);
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn refresh_keeps_selection_and_does_not_write_preference() {
        let dir = temp_theme_dir("refresh");
        let mut context = service_context(None);
        context.connection.config_dir = dir.to_string_lossy().into_owned();
        context.connection.config_dir_candidates = vec![context.connection.config_dir.clone()];
        let original = resolve_theme_picker_selection(
            &load_theme_catalog(&[]),
            "hacker",
            &ThemePluginSettings::default(),
        )
        .expect("original");
        let plugin = ThemePlugin {
            selection: std::sync::Mutex::new(Some(LiveTheme {
                snapshot: control_contract::theme_control_v1::Snapshot {
                    revision: 4,
                    selection: ThemeSelection::Preset {
                        name: "hacker".into(),
                    },
                },
                resolved: original,
            })),
            ..ThemePlugin::default()
        };
        std::fs::create_dir_all(dir.join("themes")).expect("themes");
        std::fs::write(dir.join("themes/hacker.toml"), "foreground = '#112233'")
            .expect("theme file");
        let router: TestServiceRouter = Arc::new(|_, _, capability, _, _, operation, _| {
            assert_ne!(
                capability, "bmux.storage",
                "refresh must not write preferences"
            );
            assert_eq!(operation, "apply-theme-extension");
            encode_service_message(&Ok::<
                (),
                bmux_decoration_plugin_api::decoration_state::ValidationResult,
            >(()))
        });
        let _router = install_test_service_router(router);
        let snapshot = plugin.refresh_live_theme(&context, 4).expect("refresh");
        assert_eq!(snapshot.revision, 5);
        assert_eq!(
            snapshot.selection,
            ThemeSelection::Preset {
                name: "hacker".into()
            }
        );
        let live = plugin
            .selection
            .lock()
            .expect("state")
            .clone()
            .expect("live");
        assert_eq!(live.resolved.appearance.foreground, "#112233");
        assert!(plugin.refresh_live_theme(&context, 4).is_err());
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn configured_refresh_reads_fallback_config_directory() {
        let primary = temp_theme_dir("refresh-primary");
        let fallback = temp_theme_dir("refresh-fallback");
        std::fs::write(fallback.join("bmux.toml"), "[plugins.settings.\"bmux.theme\"]\nappearance_themes = ['hacker']\ncomponent_themes = ['minimal']\n").expect("config");
        let mut context = service_context(None);
        context.connection.config_dir = primary.to_string_lossy().into_owned();
        context.connection.config_dir_candidates = vec![
            context.connection.config_dir.clone(),
            fallback.to_string_lossy().into_owned(),
        ];
        let original = resolve_picker_value(
            &load_theme_catalog(&[]),
            CONFIGURED_SELECTION,
            &ThemePluginSettings::default(),
        )
        .expect("original");
        let plugin = ThemePlugin {
            selection: std::sync::Mutex::new(Some(LiveTheme {
                snapshot: control_contract::theme_control_v1::Snapshot {
                    revision: 1,
                    selection: ThemeSelection::Configured,
                },
                resolved: original,
            })),
            ..ThemePlugin::default()
        };
        let router: TestServiceRouter = Arc::new(|_, _, capability, _, _, _, _| {
            assert_eq!(capability, "bmux.decoration.write");
            encode_service_message(&Ok::<
                (),
                bmux_decoration_plugin_api::decoration_state::ValidationResult,
            >(()))
        });
        let _router = install_test_service_router(router);
        plugin
            .refresh_live_theme(&context, 1)
            .expect("refresh fallback");
        let live = plugin
            .selection
            .lock()
            .expect("state")
            .clone()
            .expect("live");
        assert_eq!(live.resolved.appearance.foreground, "#39ff14");
        assert_eq!(live.snapshot.selection, ThemeSelection::Configured);
        assert_eq!(
            plugin
                .current_settings(&context)
                .expect("refreshed settings")
                .appearance_themes,
            vec!["hacker"]
        );
    }

    #[test]
    fn preview_rejects_foreign_owner_and_competing_mutations() {
        let plugin = ThemePlugin::default();
        let mut context = service_context(None);
        context.caller_client_id = Some(
            "00000000-0000-0000-0000-000000000001"
                .parse()
                .expect("uuid"),
        );
        context.request.service.interface_id = "theme-control-v1".into();
        context.request.operation = "current".into();
        assert!(plugin.invoke_service(context.clone()).error.is_none());
        let token = plugin.begin_preview(&context, 0).expect("preview");
        assert!(plugin.begin_preview(&context, 0).is_err());
        assert!(
            plugin
                .select_live_theme(&context, 0, ThemeSelection::Configured)
                .is_err()
        );
        assert!(plugin.refresh_live_theme(&context, 0).is_err());
        context.caller_client_id = Some(
            "00000000-0000-0000-0000-000000000002"
                .parse()
                .expect("uuid"),
        );
        assert!(plugin.cancel_preview(&context, token).is_err());
        assert!(
            plugin
                .preview_theme(&context, token, &ThemeSelection::Configured)
                .is_err()
        );
        assert!(plugin.preview.lock().expect("preview state").is_some());
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn legacy_selection_preview_refresh_and_reconnect_lifecycle() {
        let dir = temp_theme_dir("selection-lifecycle");
        let settings_text = "persistence = 'persist_between_connects'\nthemes = ['minimal']\n";
        std::fs::write(
            dir.join("bmux.toml"),
            format!("[plugins.settings.\"bmux.theme\"]\n{settings_text}"),
        )
        .expect("config");
        let mut context = service_context(Some(toml::from_str(settings_text).expect("settings")));
        context.connection.config_dir = dir.display().to_string();
        context.connection.config_dir_candidates = vec![dir.display().to_string()];
        context.caller_client_id = Some(
            "00000000-0000-0000-0000-000000000001"
                .parse()
                .expect("uuid"),
        );
        context.request.service.interface_id = "theme-control-v1".into();
        context.request.operation = "current".into();
        let stored = Arc::new(Mutex::new(b"hacker".to_vec()));
        let writes = Arc::new(Mutex::new(0));
        let observed_store = stored.clone();
        let observed_writes = writes.clone();
        let decoration = bmux_decoration_plugin::DecorationPlugin::new();
        let dispatch_context = context.clone();
        let router: TestServiceRouter = Arc::new(
            move |caller, caller_client, capability, kind, interface, operation, payload| {
                if capability == "bmux.storage" {
                    return match operation {
                        "get" => encode_service_message(&StorageGetResponse {
                            value: Some(observed_store.lock().expect("storage").clone()),
                        }),
                        "set" => {
                            let request: StorageSetRequest =
                                decode_service_message(&payload).expect("set");
                            assert_eq!(request.key.as_str(), STORAGE_SELECTED_APPEARANCE);
                            *observed_store.lock().expect("storage") = request.value;
                            *observed_writes.lock().expect("writes") += 1;
                            encode_service_message(&())
                        }
                        _ => panic!("unexpected storage operation"),
                    };
                }
                assert_eq!(capability, "bmux.decoration.write");
                let mut routed = dispatch_context.clone();
                assert_eq!(caller, "bmux.theme");
                routed.request.caller_plugin_id = caller.into();
                routed.caller_client_id = caller_client;
                routed.request.service.kind = kind;
                routed.request.service.interface_id = interface.into();
                routed.request.operation = operation.into();
                routed.request.payload = payload;
                let response = decoration.invoke_service(routed);
                assert!(response.error.is_none(), "{response:?}");
                Ok(response.payload)
            },
        );
        let _router = install_test_service_router(router);
        let plugin = ThemePlugin::default();
        let response = plugin.invoke_service(context.clone());
        assert!(response.error.is_none());
        let initial: control_contract::theme_control_v1::Snapshot =
            decode_service_message(&response.payload).expect("snapshot");
        assert_eq!(initial.selection.picker_value(), "preset:hacker");
        assert_eq!(*stored.lock().expect("storage"), b"hacker");
        let token = plugin
            .begin_preview(&context, initial.revision)
            .expect("begin");
        plugin
            .preview_theme(&context, token, &ThemeSelection::Configured)
            .expect("preview");
        plugin.cancel_preview(&context, token).expect("cancel");
        assert_eq!(*writes.lock().expect("writes"), 0);
        let token = plugin
            .begin_preview(&context, initial.revision)
            .expect("begin again");
        plugin
            .preview_theme(&context, token, &ThemeSelection::Configured)
            .expect("preview again");
        let committed = plugin
            .confirm_preview(&context, token, ThemeSelection::Configured)
            .expect("confirm");
        assert_eq!(committed.selection, ThemeSelection::Configured);
        assert_eq!(*writes.lock().expect("writes"), 1);
        let record: serde_json::Value =
            serde_json::from_slice(&stored.lock().expect("storage")).expect("versioned record");
        assert_eq!(record["version"], 1);
        assert!(record["preset"].is_null());
        let refreshed = plugin
            .refresh_live_theme(&context, committed.revision)
            .expect("refresh");
        assert_eq!(refreshed.selection, ThemeSelection::Configured);
        assert_eq!(refreshed.revision, committed.revision + 1);
        assert_eq!(*writes.lock().expect("writes"), 1);
        let reconnected = ThemePlugin::default().invoke_service(context);
        let snapshot: control_contract::theme_control_v1::Snapshot =
            decode_service_message(&reconnected.payload).expect("reconnected snapshot");
        assert_eq!(snapshot.selection, ThemeSelection::Configured);
        assert_eq!(*writes.lock().expect("writes"), 1);
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn preview_confirmation_commits_once_and_releases_owner() {
        let plugin = ThemePlugin::default();
        let mut context = service_context(None);
        context.caller_client_id = Some(
            "00000000-0000-0000-0000-000000000001"
                .parse()
                .expect("uuid"),
        );
        context.request.service.interface_id = "theme-control-v1".into();
        context.request.operation = "current".into();
        assert!(plugin.invoke_service(context.clone()).error.is_none());
        let token = plugin.begin_preview(&context, 0).expect("preview");
        let router: TestServiceRouter = Arc::new(|_, _, capability, _, _, _, _| {
            assert_eq!(capability, "bmux.decoration.write");
            encode_service_message(&Ok::<
                (),
                bmux_decoration_plugin_api::decoration_state::ValidationResult,
            >(()))
        });
        let _router = install_test_service_router(router);
        let choice = ThemeSelection::Preset {
            name: "hacker".into(),
        };
        plugin
            .preview_theme(&context, token, &choice)
            .expect("preview theme");
        assert_eq!(
            plugin
                .selection
                .lock()
                .expect("state")
                .as_ref()
                .expect("live")
                .snapshot
                .revision,
            0
        );
        let committed = plugin
            .confirm_preview(&context, token, choice.clone())
            .expect("confirm");
        assert_eq!(committed.selection, choice);
        assert_eq!(committed.revision, 1);
        assert!(plugin.preview.lock().expect("preview").is_none());
        assert!(plugin.confirm_preview(&context, token, choice).is_err());
        assert!(plugin.cancel_preview(&context, token).is_err());
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn dropped_picker_guard_cancels_owned_preview() {
        let cancelled = Arc::new(Mutex::new(Vec::new()));
        let observed = cancelled.clone();
        let router: TestServiceRouter =
            Arc::new(move |_, _, capability, _, interface, operation, payload| {
                assert_eq!(capability, "bmux.theme.write");
                assert_eq!(interface, "theme-control-v1");
                assert_eq!(operation, "cancel-preview");
                let request: control_contract::theme_control_v1::client::CancelPreviewRequest =
                    decode_service_message(&payload).expect("request");
                observed.lock().expect("cancelled").push(request.token);
                encode_service_message(&Ok::<(), String>(()))
            });
        let _router = install_test_service_router(router);
        drop(PickerPreviewGuard {
            context: picker_context(),
            token: Some(7),
        });
        drop(PickerPreviewGuard {
            context: picker_context(),
            token: None,
        });
        assert_eq!(&*cancelled.lock().expect("cancelled"), &[7]);
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn failed_preview_restoration_blocks_authoritative_queries() {
        let plugin = ThemePlugin::default();
        let mut context =
            service_context(Some(toml::from_str("theme = 'hacker'").expect("settings")));
        context.caller_client_id = Some(
            "00000000-0000-0000-0000-000000000001"
                .parse()
                .expect("uuid"),
        );
        context.request.service.interface_id = "theme-control-v1".into();
        context.request.operation = "current".into();
        assert!(plugin.invoke_service(context.clone()).error.is_none());
        let checkpoint_router = install_test_service_router(Arc::new(|_, _, _, _, _, _, _| {
            encode_service_message(&Ok::<(), String>(()))
        }));
        let token = plugin.begin_preview(&context, 0).expect("preview");
        drop(checkpoint_router);
        let router: TestServiceRouter = Arc::new(|_, _, _, _, _, _, _| {
            encode_service_message(&Err::<(), _>(
                bmux_decoration_plugin_api::decoration_state::ValidationResult::Errors {
                    errors: Vec::new(),
                },
            ))
        });
        let _router = install_test_service_router(router);
        assert!(plugin.cancel_preview(&context, token).is_err());
        assert!(
            plugin
                .recovery_required
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert!(plugin.invoke_service(context).error.is_some());
    }

    #[tokio::test]
    async fn detached_owner_releases_server_preview() {
        let bus = bmux_plugin::global_event_bus();
        bus.register_channel::<bmux_clients_plugin_api::clients_events::ClientEvent>(
            bmux_clients_plugin_api::clients_events::EVENT_KIND,
        );
        let mut plugin = ThemePlugin::default();
        plugin.watch_detach(&lifecycle_context(None));
        let mut context = service_context(None);
        let id = "00000000-0000-0000-0000-000000000001"
            .parse()
            .expect("uuid");
        context.caller_client_id = Some(id);
        context.request.service.interface_id = "theme-control-v1".into();
        context.request.operation = "current".into();
        assert!(plugin.invoke_service(context.clone()).error.is_none());
        plugin.begin_preview(&context, 0).expect("begin");
        bus.emit(
            &bmux_clients_plugin_api::clients_events::EVENT_KIND,
            bmux_clients_plugin_api::clients_events::ClientEvent::Detached { client_id: id },
        )
        .expect("detach");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if plugin.preview.lock().expect("preview").is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("server cleanup");
    }

    #[tokio::test]
    async fn expired_preview_is_cleaned_without_clients_event_stream() {
        let mut plugin = ThemePlugin::default();
        plugin.watch_detach(&lifecycle_context(None));
        let mut context = service_context(None);
        context.caller_client_id = Some(
            "00000000-0000-0000-0000-000000000001"
                .parse()
                .expect("uuid"),
        );
        context.request.service.interface_id = "theme-control-v1".into();
        context.request.operation = "current".into();
        assert!(plugin.invoke_service(context.clone()).error.is_none());
        let token = plugin.begin_preview(&context, 0).expect("begin");
        plugin
            .preview
            .lock()
            .expect("preview")
            .as_mut()
            .expect("active")
            .expires_at = std::time::Instant::now();
        assert!(plugin.renew_preview(&context, token).is_err());
        assert!(
            plugin
                .preview_theme(&context, token, &ThemeSelection::Configured)
                .is_err()
        );
        assert!(
            plugin
                .confirm_preview(&context, token, ThemeSelection::Configured)
                .is_err()
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if plugin.preview.lock().expect("preview").is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expired preview cleanup");
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn lifecycle_reapply_preserves_committed_live_selection() {
        let theme = resolve_theme_picker_selection(
            &load_theme_catalog(&[]),
            "hacker",
            &ThemePluginSettings::default(),
        )
        .expect("theme");
        let context =
            lifecycle_context(Some(toml::from_str("theme = 'minimal'").expect("settings")));
        let mut plugin = ThemePlugin {
            lifecycle_context: Some(context),
            selection: std::sync::Mutex::new(Some(LiveTheme {
                snapshot: control_contract::theme_control_v1::Snapshot {
                    revision: 7,
                    selection: ThemeSelection::Preset {
                        name: "hacker".into(),
                    },
                },
                resolved: theme.clone(),
            })),
            ..ThemePlugin::default()
        };
        let router: TestServiceRouter = Arc::new(|_, _, capability, _, _, _, _| {
            assert_eq!(capability, "bmux.decoration.write");
            encode_service_message(&Ok::<
                (),
                bmux_decoration_plugin_api::decoration_state::ValidationResult,
            >(()))
        });
        let _router = install_test_service_router(router);
        plugin
            .handle_event(PluginEvent {
                kind: PluginEventKind::from_owned("bmux.core/server_started".into()),
                payload: serde_json::json!({}),
            })
            .expect("reapply");
        assert_eq!(
            active_runtime_appearance(&service_context(None)).expect("appearance"),
            theme.appearance
        );
        assert_eq!(
            plugin
                .selection
                .lock()
                .expect("state")
                .as_ref()
                .expect("live")
                .snapshot
                .revision,
            7
        );
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn rejected_preview_restores_original_before_returning() {
        let plugin = ThemePlugin::default();
        let mut context =
            service_context(Some(toml::from_str("theme = 'hacker'").expect("settings")));
        context.caller_client_id = Some(
            "00000000-0000-0000-0000-000000000001"
                .parse()
                .expect("uuid"),
        );
        context.request.service.interface_id = "theme-control-v1".into();
        context.request.operation = "current".into();
        assert!(plugin.invoke_service(context.clone()).error.is_none());
        let checkpoint_router = install_test_service_router(Arc::new(|_, _, _, _, _, _, _| {
            encode_service_message(&Ok::<(), String>(()))
        }));
        let token = plugin.begin_preview(&context, 0).expect("begin");
        drop(checkpoint_router);
        let calls = Arc::new(Mutex::new(0));
        let observed = calls.clone();
        let router: TestServiceRouter = Arc::new(move |_, _, capability, _, _, operation, _| {
            assert_eq!(capability, "bmux.decoration.write");
            if operation == "restore-script-state" {
                *observed.lock().expect("count") += 1;
                return encode_service_message(&Ok::<(), String>(()));
            }
            let reject = {
                let mut count = observed.lock().expect("count");
                *count += 1;
                *count == 1
            };
            let result: Result<(), bmux_decoration_plugin_api::decoration_state::ValidationResult> =
                if reject {
                    Err(
                        bmux_decoration_plugin_api::decoration_state::ValidationResult::Errors {
                            errors: Vec::new(),
                        },
                    )
                } else {
                    Ok(())
                };
            encode_service_message(&result)
        });
        let _router = install_test_service_router(router);
        assert!(
            plugin
                .preview_theme(
                    &context,
                    token,
                    &ThemeSelection::Preset {
                        name: "minimal".into()
                    }
                )
                .is_err()
        );
        assert_eq!(*calls.lock().expect("count"), 2);
        assert!(plugin.preview.lock().expect("preview").is_none());
        assert!(
            !plugin
                .recovery_required
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert_eq!(
            active_runtime_appearance(&context)
                .expect("appearance")
                .foreground,
            "#39ff14"
        );
    }

    #[test]
    fn invalid_theme_settings_are_rejected_by_control_initialization() {
        let plugin = ThemePlugin::default();
        let mut context = service_context(Some(
            toml::from_str("appearance_themes = 42").expect("valid TOML"),
        ));
        context.request.service.interface_id = "theme-control-v1".into();
        context.request.operation = "current".into();
        assert!(plugin.invoke_service(context).error.is_some());
        assert!(plugin.selection.lock().expect("selection").is_none());
        assert!(try_parse_settings(Some(&toml::Value::String("bad".into()))).is_err());
        assert!(try_parse_settings(None).is_ok());
    }

    #[test]
    fn component_settings_mutation_rejects_stale_revision() {
        let plugin = ThemePlugin::default();
        let mut context = service_context(Some(
            toml::from_str("theme = 'performance'").expect("settings"),
        ));
        context.request.service.interface_id = "theme-control-v1".into();
        context.request.operation = "current".into();
        assert!(plugin.invoke_service(context.clone()).error.is_none());
        assert_eq!(
            plugin
                .set_component_settings(&context, 7, "performance", b"{}")
                .expect_err("stale"),
            "stale theme revision"
        );
        assert!(
            plugin
                .set_component_settings(&context, 0, "unknown", b"{}")
                .is_err()
        );
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn authoritative_resolution_rejects_corrupt_component_settings() {
        let mut catalog = load_theme_catalog(&[]);
        let mut theme = performance_theme_with_component();
        theme
            .settings
            .providers
            .insert("performance".into(), performance_settings_provider());
        theme.settings.component_settings.insert(
            "performance".into(),
            ThemeComponentSettingsSpec {
                components: vec!["performance.border".into()],
            },
        );
        upsert_theme_catalog_entry(&mut catalog, "performance".into(), theme);
        let router: TestServiceRouter = Arc::new(|_, _, _, _, _, _, _| {
            encode_service_message(&StorageGetResponse {
                value: Some(b"not json".to_vec()),
            })
        });
        let _router = install_test_service_router(router);
        let result = resolve_live_selection(
            &service_context(None),
            &catalog,
            &ThemeSelection::Preset {
                name: "performance".into(),
            },
            &ThemePluginSettings::default(),
        );
        assert!(result.is_err());
        assert!(
            resolve_live_selection(
                &service_context(None),
                &load_theme_catalog(&[]),
                &ThemeSelection::Configured,
                &ThemePluginSettings::default()
            )
            .is_ok()
        );
    }

    #[test]
    fn scoped_theme_settings_probe_fallback_and_reject_invalid_values() {
        let primary = temp_theme_dir("scope-primary");
        let fallback = temp_theme_dir("scope-fallback");
        let mut context = service_context(None);
        context.connection.config_dir = primary.to_string_lossy().into_owned();
        context.connection.config_dir_candidates = vec![
            context.connection.config_dir.clone(),
            fallback.to_string_lossy().into_owned(),
        ];
        let path = fallback.join("bmux.toml");
        std::fs::write(
            &path,
            "[plugins.settings.\"bmux.theme\"]\ntheme = 'hacker'\n",
        )
        .expect("config");
        let target = ConfigScopeTarget {
            name: "pane".into(),
            attributes: BTreeMap::new(),
        };
        let settings =
            scoped_theme_settings_for_target(&context, target.clone()).expect("fallback");
        assert_eq!(settings.theme.as_deref(), Some("hacker"));
        std::fs::write(&path, "[plugins.settings.\"bmux.theme\"]\nthemes = 42\n")
            .expect("invalid settings");
        assert!(scoped_theme_settings_for_target(&context, target).is_err());
    }

    #[test]
    fn scoped_theme_load_preserves_process_cli_override() {
        let dir = temp_theme_dir("process-cli");
        let primary = dir.join("bmux.toml");
        let overlay = dir.join("override.toml");
        std::fs::write(
            &primary,
            "[plugins.settings.\"bmux.theme\"]\ntheme = 'minimal'\n",
        )
        .expect("primary");
        std::fs::write(
            &overlay,
            "[plugins.settings.\"bmux.theme\"]\ntheme = 'hacker'\n",
        )
        .expect("overlay");
        let _overrides = bmux_config::push_process_config_overrides(ConfigLoadOverrides {
            base_config_path: None,
            env_config_path: None,
            cli_config_path: Some(overlay),
        });
        let mut context = service_context(None);
        context.connection.config_dir = dir.to_string_lossy().into_owned();
        context.connection.config_dir_candidates = vec![context.connection.config_dir.clone()];
        let settings = scoped_theme_settings_for_target(
            &context,
            ConfigScopeTarget {
                name: "pane".into(),
                attributes: BTreeMap::new(),
            },
        )
        .expect("scoped settings");
        assert_eq!(settings.theme.as_deref(), Some("hacker"));
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn component_settings_write_failure_preserves_live_snapshot() {
        let mut catalog = load_theme_catalog(&[]);
        let mut definition = performance_theme_with_component();
        definition
            .settings
            .providers
            .insert("performance".into(), performance_settings_provider());
        definition.settings.component_settings.insert(
            "performance".into(),
            ThemeComponentSettingsSpec {
                components: vec!["performance.border".into()],
            },
        );
        upsert_theme_catalog_entry(&mut catalog, "performance".into(), definition);
        let original = resolve_theme_picker_selection(
            &catalog,
            "performance",
            &ThemePluginSettings::default(),
        )
        .expect("theme");
        let plugin = ThemePlugin {
            selection: std::sync::Mutex::new(Some(LiveTheme {
                snapshot: control_contract::theme_control_v1::Snapshot {
                    revision: 4,
                    selection: ThemeSelection::Preset {
                        name: "performance".into(),
                    },
                },
                resolved: original.clone(),
            })),
            ..ThemePlugin::default()
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let router: TestServiceRouter = Arc::new(move |_, _, capability, _, _, operation, _| {
            observed.lock().expect("calls").push(operation.to_string());
            if capability == "bmux.storage" {
                return Err(bmux_plugin_sdk::PluginError::InvalidPluginId {
                    id: "injected storage failure".into(),
                });
            }
            encode_service_message(&Ok::<
                (),
                bmux_decoration_plugin_api::decoration_state::ValidationResult,
            >(()))
        });
        let _router = install_test_service_router(router);
        let context = service_context(Some(
            toml::from_str("persistence = 'persist_between_connects'").expect("settings"),
        ));
        assert!(
            plugin
                .set_component_settings(&context, 4, "performance", br#"{"color":"red"}"#)
                .is_err()
        );
        let live = plugin
            .selection
            .lock()
            .expect("state")
            .clone()
            .expect("live");
        assert_eq!(live.snapshot.revision, 4);
        assert_eq!(live.resolved.plugins, original.plugins);
        assert_eq!(
            &*calls.lock().expect("calls"),
            &["apply-theme-extension", "set", "apply-theme-extension"]
        );
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn failed_selection_and_refresh_reset_new_provider() {
        for refresh in [false, true] {
            let dir = temp_theme_dir("provider-rollback");
            std::fs::create_dir_all(dir.join("themes")).expect("themes");
            std::fs::write(
                dir.join("themes/rollback.toml"),
                r#"
[settings.providers.test.apply_settings]
capability = "example.settings"
interface_id = "settings-v1"
kind = "command"
operation = "apply"
[settings.providers.test.reset_settings]
capability = "example.settings"
interface_id = "settings-v1"
kind = "command"
operation = "reset"
"#,
            )
            .expect("theme");
            let settings_text = "appearance_themes = ['rollback']\ncomponent_themes = ['rollback']\n[theme_settings.test]\nvalue = 1\n";
            std::fs::write(
                dir.join("bmux.toml"),
                format!(
                    "[plugins.settings.\"bmux.theme\"]\n{}",
                    settings_text.replace(
                        "[theme_settings.test]",
                        "[plugins.settings.\"bmux.theme\".theme_settings.test]"
                    )
                ),
            )
            .expect("config");
            let mut context =
                service_context(Some(toml::from_str(settings_text).expect("settings")));
            context.connection.config_dir = dir.display().to_string();
            context.connection.config_dir_candidates = vec![dir.display().to_string()];
            let original = resolve_picker_value(
                &load_theme_catalog(&[]),
                CONFIGURED_SELECTION,
                &ThemePluginSettings::default(),
            )
            .expect("original theme");
            let plugin = ThemePlugin {
                selection: Mutex::new(Some(LiveTheme {
                    snapshot: control_contract::theme_control_v1::Snapshot {
                        revision: 4,
                        selection: ThemeSelection::Configured,
                    },
                    resolved: original,
                })),
                ..ThemePlugin::default()
            };
            let calls = Arc::new(Mutex::new(Vec::new()));
            let observed = calls.clone();
            let router: TestServiceRouter =
                Arc::new(move |_, _, capability, _, _, operation, _| {
                    if capability == "example.settings" {
                        observed.lock().expect("calls").push(operation.to_string());
                        if operation == "apply" {
                            return Err(bmux_plugin_sdk::PluginError::InvalidPluginId {
                                id: "injected partial apply failure".into(),
                            });
                        }
                        assert_eq!(operation, "reset");
                        return encode_service_message(&());
                    }
                    assert_eq!(operation, "apply-theme-extension");
                    encode_service_message(&Ok::<
                        (),
                        bmux_decoration_plugin_api::decoration_state::ValidationResult,
                    >(()))
                });
            let _router = install_test_service_router(router);
            let result = if refresh {
                plugin.refresh_live_theme(&context, 4)
            } else {
                plugin.select_live_theme(&context, 4, ThemeSelection::Configured)
            };
            assert!(result.is_err(), "refresh={refresh}: {result:?}");
            assert_eq!(*calls.lock().expect("calls"), ["apply", "reset"]);
            let live = plugin
                .selection
                .lock()
                .expect("state")
                .clone()
                .expect("live");
            assert_eq!(live.snapshot.revision, 4);
            assert_eq!(live.snapshot.selection, ThemeSelection::Configured);
            assert!(live.resolved.external_payloads.is_empty());
            assert!(
                !plugin
                    .recovery_required
                    .load(std::sync::atomic::Ordering::Acquire)
            );
        }
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn invalid_refresh_preserves_retained_theme_without_side_effects() {
        let dir = temp_theme_dir("invalid-refresh");
        std::fs::create_dir_all(dir.join("themes")).expect("themes");
        std::fs::write(dir.join("themes/hacker.toml"), "foreground = [").expect("broken theme");
        let original = resolve_theme_picker_selection(
            &load_theme_catalog(&[]),
            "hacker",
            &ThemePluginSettings::default(),
        )
        .expect("original");
        let plugin = ThemePlugin {
            selection: std::sync::Mutex::new(Some(LiveTheme {
                snapshot: control_contract::theme_control_v1::Snapshot {
                    revision: 9,
                    selection: ThemeSelection::Preset {
                        name: "hacker".into(),
                    },
                },
                resolved: original.clone(),
            })),
            ..ThemePlugin::default()
        };
        let mut context = service_context(None);
        context.connection.config_dir = dir.to_string_lossy().into_owned();
        context.connection.config_dir_candidates = vec![context.connection.config_dir.clone()];
        let router: TestServiceRouter =
            Arc::new(|_, _, _, _, _, _, _| panic!("invalid refresh must not invoke services"));
        let _router = install_test_service_router(router);
        assert!(plugin.refresh_live_theme(&context, 9).is_err());
        let retained = plugin
            .selection
            .lock()
            .expect("selection")
            .clone()
            .expect("live");
        assert_eq!(retained.snapshot.revision, 9);
        assert_eq!(retained.resolved.appearance, original.appearance);
        assert_eq!(retained.resolved.plugins, original.plugins);
        assert!(
            !plugin
                .recovery_required
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn restoring_undecorated_preview_clears_added_decoration_owner() {
        let original = resolve_picker_value(
            &load_theme_catalog(&[]),
            CONFIGURED_SELECTION,
            &ThemePluginSettings::default(),
        )
        .expect("default");
        assert!(!original.plugins.contains_key("bmux.decoration"));
        let preview = ThemePreview {
            displayed: original.clone(),
            token: 1,
            owner: "owner".into(),
            original: LiveTheme {
                snapshot: control_contract::theme_control_v1::Snapshot {
                    revision: 0,
                    selection: ThemeSelection::Configured,
                },
                resolved: original,
            },
            owners: BTreeSet::from(["bmux.decoration".into()]),
            expires_at: std::time::Instant::now(),
        };
        let calls = Arc::new(Mutex::new(0));
        let observed = calls.clone();
        let router: TestServiceRouter =
            Arc::new(move |_, _, capability, _, _, operation, payload| {
                assert_eq!(capability, "bmux.decoration.write");
                assert_eq!(operation, "apply-theme-extension");
                let request: ApplyThemeExtensionArgs =
                    decode_service_message(&payload).expect("request");
                assert!(request.toml.is_empty());
                *observed.lock().expect("calls") += 1;
                encode_service_message(&Ok::<
                    (),
                    bmux_decoration_plugin_api::decoration_state::ValidationResult,
                >(()))
            });
        let _router = install_test_service_router(router);
        restore_preview_checkpoint(&service_context(None), &preview, &[]).expect("restore");
        assert_eq!(*calls.lock().expect("calls"), 1);
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn committed_selection_is_not_reverted_after_checkpoint_disposal_failure() {
        let plugin = ThemePlugin::default();
        let mut context =
            service_context(Some(toml::from_str("theme = 'hacker'").expect("settings")));
        context.caller_client_id = Some(
            "00000000-0000-0000-0000-000000000001"
                .parse()
                .expect("uuid"),
        );
        context.request.service.interface_id = "theme-control-v1".into();
        context.request.operation = "current".into();
        let router: TestServiceRouter = Arc::new(|_, _, _, _, _, operation, _| {
            if operation == "discard-script-checkpoint" {
                return encode_service_message(&Err::<(), String>("disposal failure".into()));
            }
            if operation == "checkpoint-script-state" {
                return encode_service_message(&Ok::<(), String>(()));
            }
            encode_service_message(&Ok::<
                (),
                bmux_decoration_plugin_api::decoration_state::ValidationResult,
            >(()))
        });
        let _router = install_test_service_router(router);
        assert!(plugin.invoke_service(context.clone()).error.is_none());
        let token = plugin.begin_preview(&context, 0).expect("begin");
        assert!(
            plugin
                .confirm_preview(
                    &context,
                    token,
                    ThemeSelection::Preset {
                        name: "minimal".into()
                    }
                )
                .is_err()
        );
        assert!(plugin.preview.lock().expect("preview").is_none());
        assert!(
            plugin
                .recovery_required
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert_eq!(
            plugin
                .selection
                .lock()
                .expect("state")
                .as_ref()
                .expect("live")
                .snapshot
                .revision,
            1
        );
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn failed_confirmation_restores_checkpoint_and_releases_preview() {
        let plugin = ThemePlugin::default();
        let mut context = service_context(Some(
            toml::from_str("theme = 'hacker'\npersistence = 'persist_between_connects'")
                .expect("settings"),
        ));
        context.caller_client_id = Some(
            "00000000-0000-0000-0000-000000000001"
                .parse()
                .expect("uuid"),
        );
        context.request.service.interface_id = "theme-control-v1".into();
        context.request.operation = "current".into();
        let restored = Arc::new(Mutex::new(0));
        let observed = restored.clone();
        let router: TestServiceRouter = Arc::new(
            move |_, _, capability, _, _, operation, _| match (capability, operation) {
                ("bmux.storage", "get") => {
                    encode_service_message(&StorageGetResponse { value: None })
                }
                ("bmux.storage", "set") => Err(bmux_plugin_sdk::PluginError::InvalidPluginId {
                    id: "write failure".into(),
                }),
                (_, "checkpoint-script-state") => encode_service_message(&Ok::<(), String>(())),
                (_, "restore-script-state") => {
                    *observed.lock().expect("restored") += 1;
                    encode_service_message(&Ok::<(), String>(()))
                }
                _ => encode_service_message(&Ok::<
                    (),
                    bmux_decoration_plugin_api::decoration_state::ValidationResult,
                >(())),
            },
        );
        let _router = install_test_service_router(router);
        assert!(plugin.invoke_service(context.clone()).error.is_none());
        let token = plugin.begin_preview(&context, 0).expect("begin");
        assert!(
            plugin
                .confirm_preview(
                    &context,
                    token,
                    ThemeSelection::Preset {
                        name: "minimal".into()
                    }
                )
                .is_err()
        );
        assert_eq!(*restored.lock().expect("restored"), 1);
        assert!(plugin.preview.lock().expect("preview").is_none());
        assert_eq!(
            plugin
                .selection
                .lock()
                .expect("state")
                .as_ref()
                .expect("live")
                .snapshot
                .revision,
            0
        );
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn stale_external_form_has_no_provider_or_storage_effects() {
        let original = resolve_picker_value(
            &load_theme_catalog(&[]),
            CONFIGURED_SELECTION,
            &ThemePluginSettings::default(),
        )
        .expect("theme");
        let plugin = ThemePlugin {
            selection: std::sync::Mutex::new(Some(LiveTheme {
                snapshot: control_contract::theme_control_v1::Snapshot {
                    revision: 8,
                    selection: ThemeSelection::Configured,
                },
                resolved: original,
            })),
            ..ThemePlugin::default()
        };
        let router: TestServiceRouter =
            Arc::new(|_, _, _, _, _, _, _| panic!("stale form must not invoke services"));
        let _router = install_test_service_router(router);
        assert_eq!(
            plugin
                .apply_external_form(&service_context(None), 7, "performance", b"{}")
                .expect_err("stale form"),
            "stale theme revision"
        );
        assert_eq!(
            plugin
                .selection
                .lock()
                .expect("state")
                .as_ref()
                .expect("live")
                .snapshot
                .revision,
            8
        );
        assert!(
            !plugin
                .recovery_required
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }

    #[test]
    fn external_payload_without_apply_contract_is_rejected() {
        let mut provider = performance_settings_provider();
        provider.apply_settings = None;
        let error = apply_theme_settings_provider_payload(
            &service_context(None),
            "performance",
            &provider,
            &ThemeSettingsPayload {
                json: b"{}".to_vec(),
            },
        )
        .expect_err("missing apply contract");
        assert!(error.contains("apply_settings contract"));
    }

    #[test]
    fn removing_external_payload_requires_reset_contract() {
        let next = resolve_picker_value(
            &load_theme_catalog(&[]),
            CONFIGURED_SELECTION,
            &ThemePluginSettings::default(),
        )
        .expect("theme");
        let mut previous = next.clone();
        previous.external_payloads.insert(
            "provider".into(),
            ThemeSettingsPayload {
                json: b"{}".to_vec(),
            },
        );
        assert!(validate_provider_transition(&previous, &next).is_err());
        assert!(validate_provider_transition(&previous, &previous).is_ok());
    }

    #[test]
    #[allow(clippy::result_large_err)] // TestServiceRouter fixes the external error type.
    fn removed_provider_uses_declared_reset_endpoint() {
        let next = resolve_picker_value(
            &load_theme_catalog(&[]),
            CONFIGURED_SELECTION,
            &ThemePluginSettings::default(),
        )
        .expect("theme");
        let mut previous = next.clone();
        previous.external_payloads.insert(
            "test".into(),
            ThemeSettingsPayload {
                json: b"{}".to_vec(),
            },
        );
        previous.settings.providers.insert(
            "test".into(),
            ThemeSettingsProviderSpec {
                reset_settings: Some(ThemeSettingsEndpoint {
                    capability: "example.settings".into(),
                    interface_id: "settings-v2".into(),
                    operation: "reset".into(),
                    kind: ThemeSettingsServiceKind::Command,
                }),
                ..ThemeSettingsProviderSpec::default()
            },
        );
        let calls = Arc::new(Mutex::new(0));
        let observed = calls.clone();
        let router: TestServiceRouter = Arc::new(
            move |_, _, capability, kind, interface, operation, payload| {
                assert_eq!(capability, "example.settings");
                assert_eq!(kind, ServiceKind::Command);
                assert_eq!(interface, "settings-v2");
                assert_eq!(operation, "reset");
                decode_service_message::<()>(&payload).expect("unit request");
                *observed.lock().expect("calls") += 1;
                encode_service_message(&())
            },
        );
        let _router = install_test_service_router(router);
        reset_removed_providers(&service_context(None), &previous, &next).expect("reset");
        assert_eq!(*calls.lock().expect("calls"), 1);

        // A provider introduced only by a preview must also be reset on cancel,
        // without persisting a selection or advancing the authoritative revision.
        let mut original = next;
        original.plugins.clear();
        let live = LiveTheme {
            snapshot: control_contract::theme_control_v1::Snapshot {
                revision: 7,
                selection: ThemeSelection::Configured,
            },
            resolved: original,
        };
        let mut context = service_context(None);
        context.caller_client_id = Some(
            "00000000-0000-0000-0000-000000000001"
                .parse()
                .expect("uuid"),
        );
        let plugin = ThemePlugin {
            selection: Mutex::new(Some(live.clone())),
            preview: Arc::new(Mutex::new(Some(ThemePreview {
                token: 1,
                owner: context.caller_client_id.expect("owner").to_string(),
                original: live,
                displayed: previous,
                owners: BTreeSet::new(),
                expires_at: std::time::Instant::now() + std::time::Duration::from_secs(30),
            }))),
            ..ThemePlugin::default()
        };
        plugin.cancel_preview(&context, 1).expect("cancel preview");
        assert_eq!(*calls.lock().expect("calls"), 2);
        assert!(plugin.preview.lock().expect("preview").is_none());
        let selection = plugin.selection.lock().expect("selection");
        let snapshot = &selection.as_ref().expect("live selection").snapshot;
        assert_eq!(snapshot.revision, 7);
        assert_eq!(snapshot.selection, ThemeSelection::Configured);
        drop(selection);
    }

    #[test]
    fn performance_provider_is_not_required_for_theme_activation() {
        let declaration: toml::Value =
            toml::from_str(include_str!("../plugin.toml")).expect("valid manifest");
        let required = declaration["required_capabilities"]
            .as_array()
            .expect("required capabilities");
        assert!(
            !required
                .iter()
                .any(|value| value.as_str() == Some("bmux.performance.write"))
        );
        let optional = declaration["optional_capabilities"]
            .as_array()
            .expect("optional capabilities");
        assert!(
            optional
                .iter()
                .any(|value| value.as_str() == Some("bmux.performance.write"))
        );
    }

    #[test]
    fn active_appearance_service_uses_declared_theme() {
        let plugin = ThemePlugin::default();
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
    fn malformed_stored_selection_does_not_resolve_configured_appearance() {
        let _router = install_persisted_theme_router(Some(r#"{"version":1}"#));
        let context = service_context(Some(
            toml::from_str("persistence = 'persist_between_connects'").expect("settings"),
        ));
        assert!(configured_theme(&context).is_none());
        let response = ThemePlugin::default().invoke_service(context);
        assert!(response.error.is_some());
    }

    #[test]
    fn configured_record_restores_declared_split_stacks() {
        let _router = install_persisted_theme_router(Some(r#"{"version":1,"preset":null}"#));
        let context = service_context(Some(
            toml::from_str(
                r#"
            persistence = "persist_between_connects"
            appearance_themes = ["performance", "mode-aware"]
            component_themes = ["performance", "pulse-border"]
        "#,
            )
            .expect("settings"),
        ));
        let active = configured_theme(&context).expect("configured selection");
        assert_ne!(active.source, ActiveThemeSource::Persisted);
        let settings = try_parse_settings(context.settings.as_ref()).expect("valid settings");
        let expected =
            resolve_picker_value(&load_theme_catalog(&[]), CONFIGURED_SELECTION, &settings)
                .expect("configured");
        assert_eq!(active.theme.plugins, expected.plugins);
        assert_eq!(active.theme.appearance, expected.appearance);
    }

    #[test]
    fn active_appearance_service_uses_persisted_theme_when_enabled() {
        let _router = install_persisted_theme_router(Some("hacker"));
        let plugin = ThemePlugin::default();
        let context = service_context(Some(toml::Value::Table(toml::map::Map::from_iter([
            (
                "theme".to_string(),
                toml::Value::String("pulse-border".to_string()),
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
                toml::Value::String("pulse-border".to_string()),
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
    fn explicit_theme_stack_allows_persisted_preset_override() {
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

        assert_eq!(active.source, ActiveThemeSource::Persisted);
        assert_eq!(active.theme.appearance.foreground, "#39ff14");
    }

    #[test]
    fn persisted_preset_overrides_split_stacks() {
        let _router = install_persisted_theme_router(Some("hacker"));
        let settings = toml::from_str::<toml::Value>(
            r#"
            appearance_themes = ["performance", "mode-aware"]
            component_themes = ["performance", "pulse-border"]
            persistence = "persist_between_connects"
            "#,
        )
        .expect("valid settings");
        let context = service_context(Some(settings));
        let active = configured_theme(&context).expect("persisted preset should resolve");
        assert_eq!(active.source, ActiveThemeSource::Persisted);
        assert_eq!(active.theme.appearance.foreground, "#39ff14");
        let decoration = active
            .theme
            .plugins
            .get("bmux.decoration")
            .expect("decoration");
        assert!(decoration.get("components").is_none());
    }

    #[test]
    fn unknown_persisted_theme_falls_back_to_declared_theme() {
        let _router = install_persisted_theme_router(Some("missing-theme"));
        let context = service_context(Some(toml::Value::Table(toml::map::Map::from_iter([
            (
                "theme".to_string(),
                toml::Value::String("pulse-border".to_string()),
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
                toml::Value::String("pulse-border".to_string()),
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
                toml::Value::String("pulse-border".to_string()),
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
    fn theme_settings_provider_uses_storage_safe_key_for_persist_and_restore() {
        let settings = serde_json::json!({ "sample_interval_ms": 2_500 });
        let settings_bytes = serde_json::to_vec(&settings).expect("settings should encode");
        let stored_keys = Arc::new(Mutex::new(Vec::new()));
        let applied_settings = Arc::new(Mutex::new(Vec::new()));
        let provider = performance_settings_provider();
        let _router = install_theme_settings_router(
            settings_bytes,
            Arc::clone(&stored_keys),
            Arc::clone(&applied_settings),
        );
        let context = lifecycle_context(None);

        let payload = ThemeSettingsPayload::from_value(&settings).expect("settings encode");
        persist_theme_settings(&context, "performance-header", &provider, &payload);
        let persisted = read_persisted_theme_settings(&context, "performance-header", &provider)
            .expect("persisted settings should read");
        apply_theme_settings_provider_payload(
            &context,
            "performance-header",
            &provider,
            &persisted,
        )
        .expect("apply provider settings");

        let stored_keys_snapshot = {
            let stored_keys = stored_keys.lock().expect("stored key lock should hold");
            stored_keys.clone()
        };
        assert_eq!(
            stored_keys_snapshot.as_slice(),
            ["theme_settings.performance"]
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
                toml::Value::String("pulse-border".to_string()),
            ),
            (
                "persistence".to_string(),
                toml::Value::String("persist_between_connects".to_string()),
            ),
        ]))));
        let mut plugin = ThemePlugin {
            lifecycle_context: Some(context),
            ..ThemePlugin::default()
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

    fn performance_theme_with_component() -> ThemeConfig {
        toml::from_str(
            r##"
            name = "performance"
            foreground = "#ffffff"

            [plugins."bmux.decoration".focused]
            bg = ""
            fg = "#ffaf00"
            glyphs_custom = []
            gradient_from = ""
            gradient_to = ""
            style = "thick"

            [plugins."bmux.decoration".unfocused]
            bg = ""
            fg = "#444444"
            glyphs_custom = []
            gradient_from = ""
            gradient_to = ""
            style = "single-line"

            [plugins."bmux.decoration".zoomed]
            bg = ""
            fg = "#ff5f5f"
            glyphs_custom = []
            gradient_from = ""
            gradient_to = ""
            style = "double"

            [plugins."bmux.decoration"]
            script = "performance_header"

            [plugins."bmux.decoration".script_access]
            state_channels = ["bmux.performance/metrics-state"]

            [plugins."bmux.decoration".badges]
            exited = "x"
            running = ">"

            [plugins."bmux.decoration".components."performance.border"]
            script = "performance_header"
            "##,
        )
        .expect("performance theme parses")
    }

    fn pong_theme_with_component() -> ThemeConfig {
        toml::from_str(
            r##"
            name = "pong"
            foreground = "#00ffff"

            [plugins."bmux.decoration".focused]
            bg = ""
            fg = "#ffffff"
            glyphs_custom = []
            gradient_from = ""
            gradient_to = ""
            style = "rounded"

            [plugins."bmux.decoration".components."pong.ball"]
            script = "pong"
            "##,
        )
        .expect("pong theme parses")
    }

    fn split_stack_catalog() -> Vec<ThemeCatalogEntry> {
        vec![
            ThemeCatalogEntry {
                name: "performance".to_string(),
                theme: performance_theme_with_component(),
            },
            ThemeCatalogEntry {
                name: "pong".to_string(),
                theme: pong_theme_with_component(),
            },
        ]
    }

    #[test]
    fn picker_selection_ignores_configured_split_theme_providers() {
        let selected_theme: ThemeConfig = toml::from_str(
            r##"
            name = "tetris"
            foreground = "#00ffff"

            [settings.providers.tetris]
            prompt_on_select = true
            storage_key = "theme_settings.tetris"

            [settings.component_settings.tetris]
            components = ["tetris.board"]

            [plugins."bmux.decoration".components."tetris.board"]
            script = "tetris"
            "##,
        )
        .expect("selected theme parses");
        let configured_component_theme: ThemeConfig = toml::from_str(
            r##"
            name = "performance"
            foreground = "#ffffff"

            [settings.providers.performance]
            prompt_on_select = true
            storage_key = "theme_settings.performance"

            [settings.component_settings.performance]
            components = ["performance.header"]

            [plugins."bmux.decoration".components."performance.header"]
            script = "performance_header"
            "##,
        )
        .expect("configured component theme parses");
        let catalog = vec![
            ThemeCatalogEntry {
                name: "mode-aware".to_string(),
                theme: ThemeConfig::default(),
            },
            ThemeCatalogEntry {
                name: "performance".to_string(),
                theme: configured_component_theme,
            },
            ThemeCatalogEntry {
                name: "tetris".to_string(),
                theme: selected_theme,
            },
        ];
        let settings = ThemePluginSettings {
            appearance_themes: vec!["performance".to_string()],
            component_themes: vec!["performance".to_string()],
            ..ThemePluginSettings::default()
        };

        let resolved = resolve_theme_picker_selection(&catalog, "tetris", &settings)
            .expect("picker selection resolves");

        assert!(resolved.settings.providers.contains_key("tetris"));
        assert!(!resolved.settings.providers.contains_key("performance"));
        let components = resolved
            .plugins
            .get("bmux.decoration")
            .and_then(toml::Value::as_table)
            .and_then(|decoration| decoration.get("components"))
            .and_then(toml::Value::as_table)
            .expect("selected theme components exist");
        assert!(components.contains_key("tetris.board"));
        assert!(!components.contains_key("performance.header"));
    }

    #[test]
    fn deprecated_pulse_demo_alias_resolves_without_duplicate_catalog_entry() {
        let catalog = load_theme_catalog(&[]);
        assert!(catalog.iter().any(|entry| entry.name == "pulse-border"));
        assert!(!catalog.iter().any(|entry| entry.name == "pulse-demo"));
        assert_eq!(
            theme_by_name(&catalog, "pulse-demo").unwrap().name,
            "pulse-border"
        );
        assert_eq!(
            selected_index(&catalog, "pulse-demo"),
            selected_index(&catalog, "pulse-border")
        );
        let legacy = resolve_theme_stack(&catalog, &["pulse-demo".to_string()]).unwrap();
        let renamed = resolve_theme_stack(&catalog, &["pulse-border".to_string()]).unwrap();
        assert_eq!(legacy.plugins, renamed.plugins);
        let mut catalog = catalog;
        upsert_theme_catalog_entry(
            &mut catalog,
            "pulse-demo".to_string(),
            ThemeConfig::default(),
        );
        assert_eq!(
            theme_by_name(&catalog, "pulse-demo").unwrap().name,
            "pulse-demo"
        );
    }

    #[test]
    fn performance_and_pulse_compose_without_competing_scripted_borders() {
        let catalog = vec![
            ThemeCatalogEntry {
                name: "performance".to_string(),
                theme: toml::from_str(include_str!("../assets/themes/performance.toml")).unwrap(),
            },
            ThemeCatalogEntry {
                name: "pulse-border".to_string(),
                theme: toml::from_str(include_str!("../assets/themes/pulse-border.toml")).unwrap(),
            },
        ];
        let settings: ThemePluginSettings = toml::from_str(
            r#"
            appearance_themes = ["performance"]
            component_themes = ["performance", "pulse-border"]
            [components."performance.border"]
            enabled = false
            [components."pulse.border"]
            below = ["performance.header"]
            [components."pulse.border".settings]
            color-source = "performance-colors-v1"
            heat-mode = "cpu-memory"
            smoothing-ms = "500"
        "#,
        )
        .unwrap();
        let resolved = resolve_theme_stack_with_settings(&catalog, &[], &settings).unwrap();
        let decoration = &resolved.plugins["bmux.decoration"];
        let components = &decoration["components"];
        assert_eq!(
            components["performance.border"]["enabled"].as_bool(),
            Some(false)
        );
        assert_eq!(components["pulse.border"]["script"].as_str(), Some("pulse"));
        assert_eq!(
            components["pulse.border"]["animation"]["hz"].as_integer(),
            Some(30)
        );
        assert_eq!(
            components["pulse.border"]["settings"]["color-source"].as_str(),
            Some("performance-colors-v1")
        );
        assert_eq!(
            components["performance.header"]["script"].as_str(),
            Some("performance_header")
        );
        assert_eq!(
            decoration["script_access"]["state_channels"][0].as_str(),
            Some("bmux.performance/metrics-state")
        );
        assert!(components["performance.border"].get("animation").is_none());
    }

    #[test]
    fn split_stacks_keep_appearance_base_and_apply_component_targets() {
        let settings = ThemePluginSettings {
            appearance_themes: vec!["performance".to_string()],
            component_themes: vec!["performance".to_string(), "pong".to_string()],
            component_targets: BTreeMap::from([(
                "pong.*".to_string(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "kind".to_string(),
                    toml::Value::String("unfocused-panes".to_string()),
                )])),
            )]),
            ..ThemePluginSettings::default()
        };
        let resolved = resolve_theme_stack_with_settings(
            &split_stack_catalog(),
            &["performance".to_string()],
            &settings,
        )
        .expect("split stack resolves");
        let decoration = resolved
            .plugins
            .get("bmux.decoration")
            .and_then(toml::Value::as_table)
            .expect("decoration extension exists");
        let components = decoration
            .get("components")
            .and_then(toml::Value::as_table)
            .expect("components exist");

        assert_eq!(resolved.appearance.foreground, "#ffffff");
        assert_eq!(
            decoration
                .get("focused")
                .and_then(toml::Value::as_table)
                .and_then(|focused| focused.get("style"))
                .and_then(toml::Value::as_str),
            Some("thick")
        );
        assert!(components.contains_key("performance.border"));
        assert_eq!(decoration.get("script"), None);
        assert_eq!(
            decoration
                .get("script_access")
                .and_then(toml::Value::as_table)
                .and_then(|access| access.get("state_channels"))
                .and_then(toml::Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(
            components
                .get("pong.ball")
                .and_then(toml::Value::as_table)
                .and_then(|component| component.get("target"))
                .and_then(toml::Value::as_table)
                .and_then(|target| target.get("kind"))
                .and_then(toml::Value::as_str),
            Some("unfocused-panes")
        );
    }

    #[test]
    fn split_stacks_strip_appearance_only_decoration_components() {
        let settings = ThemePluginSettings {
            appearance_themes: vec!["performance".to_string()],
            component_themes: vec!["pong".to_string()],
            ..ThemePluginSettings::default()
        };
        let resolved = resolve_theme_stack_with_settings(
            &split_stack_catalog(),
            &["performance".to_string()],
            &settings,
        )
        .expect("split stack resolves");
        let components = resolved
            .plugins
            .get("bmux.decoration")
            .and_then(toml::Value::as_table)
            .and_then(|decoration| decoration.get("components"))
            .and_then(toml::Value::as_table)
            .expect("component stack components exist");

        assert!(!components.contains_key("performance.border"));
        assert!(components.contains_key("pong.ball"));
    }

    #[test]
    fn builtin_component_settings_form_defaults_follow_declared_component_settings() {
        let theme: ThemeConfig = toml::from_str(
            r#"
            [settings.providers.pong]
            storage_key = "theme_settings.pong"

            [settings.component_settings.pong]
            components = ["pong.ball", "pong.paddles"]

            [[settings.forms.pong.fields]]
            default = false
            key = "content_bounce"
            label = "Bounce off terminal content"
            type = "bool"

            [plugins."bmux.decoration".components."pong.ball".settings]
            content_bounce = "true"

            [plugins."bmux.decoration".components."pong.paddles".settings]
            content_bounce = "true"
            "#,
        )
        .expect("theme parses");
        let catalog = vec![ThemeCatalogEntry {
            name: "pong".to_string(),
            theme,
        }];
        let resolved =
            resolve_theme_stack(&catalog, &["pong".to_string()]).expect("theme resolves");
        let provider = resolved
            .settings
            .providers
            .get("pong")
            .expect("provider exists");
        let context = lifecycle_context(None);
        let defaults = effective_theme_settings_payload(
            &context,
            &resolved,
            "pong",
            provider,
            &ThemePluginSettings::default(),
        );
        let form = resolved.settings.forms.get("pong").expect("form exists");
        let request = build_builtin_theme_settings_form("pong", form, &defaults);
        let bmux_plugin_sdk::PromptField::Form { sections, .. } = request.field else {
            panic!("settings prompt should be a form");
        };
        let field = sections
            .first()
            .and_then(|section| section.fields.first())
            .expect("form field exists");
        assert_eq!(field.id, "content_bounce");
        assert_eq!(
            field.kind,
            bmux_plugin_sdk::PromptFormFieldKind::Bool { default: true }
        );

        let settings = ThemePluginSettings {
            theme_settings: BTreeMap::from([(
                "pong".to_string(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "content_bounce".to_string(),
                    toml::Value::Boolean(false),
                )])),
            )]),
            ..ThemePluginSettings::default()
        };
        let defaults =
            effective_theme_settings_payload(&context, &resolved, "pong", provider, &settings);
        let request = build_builtin_theme_settings_form("pong", form, &defaults);
        let bmux_plugin_sdk::PromptField::Form { sections, .. } = request.field else {
            panic!("settings prompt should be a form");
        };
        let field = sections
            .first()
            .and_then(|section| section.fields.first())
            .expect("form field exists");
        assert_eq!(
            field.kind,
            bmux_plugin_sdk::PromptFormFieldKind::Bool { default: false }
        );
    }

    #[test]
    fn theme_settings_can_override_declared_component_settings() {
        let theme: ThemeConfig = toml::from_str(
            r#"
            [settings.component_settings.pong]
            components = ["pong.ball", "pong.paddles"]

            [plugins."bmux.decoration".components."pong.ball"]
            script = "pong"

            [plugins."bmux.decoration".components."pong.ball".settings]
            rally_ms = "5500"
            "#,
        )
        .expect("theme parses");
        let catalog = vec![ThemeCatalogEntry {
            name: "pong".to_string(),
            theme,
        }];
        let settings = ThemePluginSettings {
            theme_settings: BTreeMap::from([(
                "pong".to_string(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "rally_ms".to_string(),
                    toml::Value::Integer(8_000),
                )])),
            )]),
            ..ThemePluginSettings::default()
        };

        let resolved =
            resolve_theme_stack_with_settings(&catalog, &["pong".to_string()], &settings)
                .expect("theme resolves");
        let decoration = resolved
            .plugins
            .get("bmux.decoration")
            .and_then(toml::Value::as_table)
            .expect("decoration extension exists");
        let components = decoration
            .get("components")
            .and_then(toml::Value::as_table)
            .expect("components exist");
        for component_id in ["pong.ball", "pong.paddles"] {
            let rally_ms = components
                .get(component_id)
                .and_then(toml::Value::as_table)
                .and_then(|component| component.get("settings"))
                .and_then(toml::Value::as_table)
                .and_then(|settings| settings.get("rally_ms"))
                .and_then(toml::Value::as_str);
            assert_eq!(rally_ms, Some("8000"));
        }
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

    #[test]
    fn cwd_scoped_active_appearance_uses_local_theme_setting() {
        let root = temp_theme_dir("scope");
        let project = root.join("project");
        std::fs::create_dir_all(&project).expect("create project dir");
        std::fs::write(root.join("bmux.toml"), "").expect("write global config");
        std::fs::write(
            project.join("bmux.toml"),
            r#"
            [plugins.settings."bmux.theme"]
            theme = "tetris"
            "#,
        )
        .expect("write local config");

        let mut context = service_context(None);
        context.connection.config_dir = root.to_string_lossy().into_owned();
        context.connection.config_dir_candidates = vec![root.to_string_lossy().into_owned()];

        let appearance =
            active_runtime_appearance_for_cwd(&context, project.to_string_lossy().as_ref())
                .expect("cwd-scoped appearance resolves");

        assert_eq!(appearance.foreground, "#d8e8ff");
        std::fs::remove_dir_all(root).ok();
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
            cancellation: bmux_plugin_sdk::CancellationToken::default(),
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
                "bmux.theme.settings".to_string(),
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

    fn performance_settings_provider() -> ThemeSettingsProviderSpec {
        ThemeSettingsProviderSpec {
            reset_settings: None,
            modal_id: Some("performance-advanced-settings".to_string()),
            storage_key: Some("theme_settings.performance".to_string()),
            prompt_on_select: Some(true),
            form: None,
            apply_form: None,
            apply_settings: Some(ThemeSettingsEndpoint {
                capability: "bmux.theme.settings".to_string(),
                interface_id: "performance-theme-settings".to_string(),
                operation: "set-settings".to_string(),
                kind: ThemeSettingsServiceKind::Command,
            }),
        }
    }

    #[allow(clippy::result_large_err)] // Test router signature is fixed by bmux_plugin test support.
    fn install_theme_settings_router(
        settings_bytes: Vec<u8>,
        stored_keys: Arc<Mutex<Vec<String>>>,
        applied_settings: Arc<Mutex<Vec<serde_json::Value>>>,
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
                        let stored_settings: serde_json::Value =
                            serde_json::from_slice(&request.value)
                                .expect("stored theme settings should decode");
                        assert_eq!(stored_settings["sample_interval_ms"], 2_500);
                        encode_service_message(&())
                    }
                    ("bmux.storage", ServiceKind::Query, "storage-query/v1", "get") => {
                        let request: StorageGetRequest = decode_service_message(&payload)
                            .expect("storage get payload should decode");
                        assert_eq!(request.key.as_str(), "theme_settings.performance");
                        encode_service_message(&StorageGetResponse {
                            value: Some(settings_bytes.clone()),
                        })
                    }
                    (
                        "bmux.theme.settings",
                        ServiceKind::Command,
                        "performance-theme-settings",
                        "set-settings",
                    ) => {
                        let settings: ThemeSettingsPayload = decode_service_message(&payload)
                            .expect("theme settings payload should decode");
                        let settings_value: serde_json::Value =
                            serde_json::from_slice(&settings.json).expect("json decodes");
                        applied_settings
                            .lock()
                            .expect("applied settings lock should hold")
                            .push(settings_value);
                        encode_service_message(&settings)
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
                    (
                        "bmux.decoration.write",
                        ServiceKind::Command,
                        "decoration-commands",
                        "apply-theme-extension",
                    ) => {
                        let request: ApplyThemeExtensionArgs = decode_service_message(&payload)
                            .expect("theme extension payload should decode");
                        applied
                            .lock()
                            .expect("applied extensions lock should hold")
                            .push(request);
                        encode_service_message(&Ok::<
                            (),
                            bmux_decoration_plugin_api::decoration_state::ValidationResult,
                        >(()))
                    }
                    other => panic!("unexpected service call: {other:?}"),
                }
            },
        );
        install_test_service_router(router)
    }
}

bmux_plugin_sdk::export_plugin!(ThemePlugin, include_str!("../plugin.toml"));
