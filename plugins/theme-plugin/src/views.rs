//! Attachment-local ownership. The default is a preference, never a live view.
use super::{
    ActiveThemeSource, ConfigScopeTarget, DetachTask, EXIT_OK, LiveTheme, NativeCommandContext,
    NativeLifecycleContext, NativeServiceContext, PluginCommandError, PluginEvent, ResolvedTheme,
    RustPlugin, ServiceResponse, ThemeSelection, ThemeView, active_theme_stack, control_contract,
    load_theme_catalog, picker_selection_name, resolve_live_selection,
    scoped_theme_settings_for_target, try_parse_settings,
};
use bmux_plugin::HostRuntimeApi;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct ThemePlugin {
    views: Arc<Mutex<BTreeMap<String, Arc<View>>>>,
    default: Mutex<Option<control_contract::theme_control_v1::Snapshot>>,
    detach_task: Option<DetachTask>,
    preview_sequence: Arc<std::sync::atomic::AtomicU64>,
}

struct View {
    // Serializes initialization, mutation, expiry, and effective queries for this view only.
    operation: Mutex<()>,
    state: ThemeView,
    context: NativeServiceContext,
    closed: std::sync::atomic::AtomicBool,
    published: Mutex<Option<super::RuntimeAppearance>>,
    explicit: std::sync::atomic::AtomicBool,
    scope: Mutex<Option<ConfigScopeTarget>>,
}

impl ThemePlugin {
    fn view(&self, context: &NativeServiceContext) -> Result<Arc<View>, ServiceResponse> {
        let owner = context
            .caller_client_id
            .filter(|id| !id.is_nil())
            .ok_or_else(|| {
                ServiceResponse::error(
                    "attachment_required",
                    "theme operation requires an attachment",
                )
            })?
            .to_string();
        let mut views = self.views.lock().map_err(|_| {
            ServiceResponse::error(
                "theme_state_poisoned",
                "theme view registry requires recovery",
            )
        })?;
        Ok(Arc::clone(views.entry(owner).or_insert_with(|| {
            Arc::new(View {
                operation: Mutex::new(()),
                state: ThemeView {
                    preview_sequence: Arc::clone(&self.preview_sequence),
                    ..ThemeView::default()
                },
                context: context.clone(),
                closed: std::sync::atomic::AtomicBool::new(false),
                published: Mutex::new(None),
                explicit: std::sync::atomic::AtomicBool::new(false),
                scope: Mutex::new(None),
            })
        })))
    }

    fn default_snapshot(
        &self,
        context: &NativeServiceContext,
    ) -> Result<control_contract::theme_control_v1::Snapshot, String> {
        let mut default = self
            .default
            .lock()
            .map_err(|_| "default requires recovery")?;
        if default.is_none() {
            let settings = try_parse_settings(context.settings.as_ref())?;
            let catalog = load_theme_catalog(&context.connection.config_dir_candidate_paths());

            *default = Some(control_contract::theme_control_v1::Snapshot {
                revision: 0,
                selection: baseline_selection(context, &settings, &catalog)?,
            });
        }
        default
            .clone()
            .ok_or_else(|| "default theme not found".into())
    }

    fn select_default(
        &self,
        context: &NativeServiceContext,
        expected: u64,
        selection: ThemeSelection,
    ) -> Result<control_contract::theme_control_v1::Snapshot, String> {
        self.default_snapshot(context)?;
        let mut default = self
            .default
            .lock()
            .map_err(|_| "default requires recovery")?;
        let current = default.as_ref().ok_or("default theme not found")?;
        if current.revision != expected {
            return Err("stale default revision".into());
        }
        let revision = expected
            .checked_add(1)
            .ok_or("default revision exhausted")?;
        let settings = try_parse_settings(context.settings.as_ref())?;
        let catalog = load_theme_catalog(&context.connection.config_dir_candidate_paths());
        // Validate without applying extension/provider state to any attachment.
        resolve_live_selection(context, &catalog, &selection, &settings)?;
        let record = super::PersistedThemeSelection {
            version: 1,
            preset: match &selection {
                ThemeSelection::Configured => None,
                ThemeSelection::Preset { name } => Some(name.clone()),
            },
        };
        context
            .storage_set(&bmux_plugin_sdk::StorageSetRequest::new(
                bmux_plugin_sdk::storage_key!("default_theme"),
                serde_json::to_vec(&record).map_err(|error| error.to_string())?,
            ))
            .map_err(|error| error.to_string())?;
        let snapshot = control_contract::theme_control_v1::Snapshot {
            revision,
            selection,
        };
        *default = Some(snapshot.clone());
        drop(default);
        Ok(snapshot)
    }

    fn watch_detach(&mut self, _context: &NativeLifecycleContext) {
        if self.detach_task.is_some() {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let views = Arc::clone(&self.views);
        let mut events = bmux_plugin::global_event_bus()
            .subscribe::<bmux_clients_plugin_api::clients_events::ClientEvent>(
                &bmux_clients_plugin_api::clients_events::EVENT_KIND,
            )
            .ok();
        self.detach_task = Some(DetachTask(handle.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                let detached = tokio::select! {
                    _ = tick.tick() => None,
                    event = async { match events.as_mut() {
                        Some(events) => events.recv().await,
                        None => std::future::pending().await,
                    }} => if let Ok(event) = event { match event.as_ref() {
                            bmux_clients_plugin_api::clients_events::ClientEvent::Detached { client_id } => Some(client_id.to_string()),
                            _ => continue,
                        }
                    } else { events = bmux_plugin::global_event_bus().subscribe::<bmux_clients_plugin_api::clients_events::ClientEvent>(&bmux_clients_plugin_api::clients_events::EVENT_KIND).ok(); None }
                };
                let mut retired = Vec::new();
                if let Some(owner) = detached
                    && let Ok(mut registry) = views.lock()
                    && let Some(view) = registry.remove(&owner) { retired.push(view); }
                // Transport liveness works even when the optional clients facade is absent.
                let sink = bmux_plugin::global_plugin_state_registry()
                    .get::<bmux_plugin_sdk::WireEventSinkHandle>()
                    .and_then(|sink| sink.read().ok().map(|sink| sink.0.clone()));
                if let Some(sink) = sink { retired.extend(retire_disconnected(&views, sink.as_ref())); }
                let snapshot = if let Ok(registry) = views.lock() {
                    registry.values().cloned().collect::<Vec<_>>()
                } else { continue; };
                // Sink work and per-view synchronization never run under the registry lock.
                for view in retired { view.close(); }
                for view in snapshot {
                    let Ok(_operation) = view.operation.lock() else { continue; };
                    view.expire();
                }
            }
        })));
    }
}

// Snapshot before consulting the sink: no transport calls under the registry lock.
// Unknown liveness is not evidence of detachment.
fn retire_disconnected(
    views: &Mutex<BTreeMap<String, Arc<View>>>,
    sink: &dyn bmux_plugin_sdk::WireEventSink,
) -> Vec<Arc<View>> {
    let snapshot = match views.lock() {
        Ok(views) => views
            .iter()
            .map(|(owner, view)| (owner.clone(), Arc::clone(view)))
            .collect::<Vec<_>>(),
        Err(_) => return Vec::new(),
    };
    let disconnected = snapshot
        .into_iter()
        .filter(|(_, view)| {
            view.context
                .caller_client_id
                .is_some_and(|owner| sink.is_connected(owner) == Some(false))
        })
        .collect::<Vec<_>>();
    let Ok(mut registry) = views.lock() else {
        return Vec::new();
    };
    let mut retired = Vec::new();
    for (owner, view) in disconnected {
        if registry
            .get(&owner)
            .is_some_and(|current| Arc::ptr_eq(current, &view))
        {
            registry.remove(&owner);
            retired.push(view);
        }
    }
    drop(registry);
    retired
}

impl View {
    fn publish(&self) {
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let result = self.effective().and_then(|theme| {
            let mut published = self
                .published
                .lock()
                .map_err(|_| "appearance publication cache poisoned")?;
            if published.as_ref() == Some(&theme.appearance) {
                return Ok(());
            }
            let payload =
                serde_json::to_vec(&theme.appearance).map_err(|error| error.to_string())?;
            let sink = bmux_plugin::global_plugin_state_registry()
                .get::<bmux_plugin_sdk::WireEventSinkHandle>()
                .ok_or("targeted appearance delivery unavailable")?;
            let sink = sink.read().map_err(|_| "appearance sink poisoned")?;
            sink.0
                .publish_retained_to(
                    self.context.caller_client_id.ok_or("attachment required")?,
                    super::RUNTIME_APPEARANCE_STATE_KIND.as_str().to_string(),
                    payload,
                )
                .map_err(|error| error.to_string())?;
            *published = Some(theme.appearance);
            drop(published);
            drop(sink);
            Ok(())
        });
        if let Err(error) = result {
            tracing::warn!(%error, "private appearance delivery failed; authoritative state remains queryable");
        }
    }

    fn close(&self) {
        // Serialize clear after any in-flight publish, preventing late re-retention.
        let Ok(_operation) = self.operation.lock() else {
            return;
        };
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(sink) = bmux_plugin::global_plugin_state_registry()
            .get::<bmux_plugin_sdk::WireEventSinkHandle>()
            && let (Ok(sink), Some(owner)) = (sink.read(), self.context.caller_client_id)
            && let Err(error) = sink
                .0
                .clear_retained_to(owner, super::RUNTIME_APPEARANCE_STATE_KIND.as_str())
        {
            tracing::warn!(%error, "private appearance cleanup failed");
        }
    }

    fn initialize(
        &self,
        context: &NativeServiceContext,
        target: Option<ConfigScopeTarget>,
    ) -> Result<(), String> {
        let mut state = self
            .state
            .selection
            .lock()
            .map_err(|_| "theme requires recovery")?;
        let mut scope = self.scope.lock().map_err(|_| "scope requires recovery")?;
        let changed = target
            .as_ref()
            .is_some_and(|target| scope.as_ref() != Some(target));
        if state.is_some() && !changed {
            return Ok(());
        }
        let revision = state.as_ref().map_or(0, |live| live.snapshot.revision);
        let settings = if let Some(target) = target.as_ref() {
            scoped_theme_settings_for_target(context, target.clone())?
        } else {
            try_parse_settings(context.settings.as_ref())?
        };
        // Latest requested baseline is independent of an explicit/preview overlay.
        *self
            .state
            .refreshed_settings
            .lock()
            .map_err(|_| "settings require recovery")? =
            Some(toml::Value::try_from(&settings).map_err(|error| error.to_string())?);
        if target.is_some() {
            scope.clone_from(&target);
        }
        if state.is_some()
            && (self.explicit.load(std::sync::atomic::Ordering::Acquire)
                || self
                    .state
                    .preview
                    .lock()
                    .map_err(|_| "preview requires recovery")?
                    .is_some())
        {
            return Ok(());
        }
        let catalog = load_theme_catalog(&context.connection.config_dir_candidate_paths());
        let selection = if self.explicit.load(std::sync::atomic::Ordering::Acquire) {
            state
                .as_ref()
                .ok_or("theme not initialized")?
                .snapshot
                .selection
                .clone()
        } else {
            baseline_selection(context, &settings, &catalog)?
        };
        let resolved = resolve_live_selection(context, &catalog, &selection, &settings)?;
        super::try_apply_theme_extensions(
            context,
            &resolved,
            &["bmux.decoration".into()],
            &context.connection.config_dir_candidates,
        )?;
        // Store scoped settings in the view so later mutations use the same baseline.
        *self
            .state
            .refreshed_settings
            .lock()
            .map_err(|_| "settings require recovery")? =
            Some(toml::Value::try_from(settings).map_err(|error| error.to_string())?);
        *state = Some(LiveTheme {
            snapshot: control_contract::theme_control_v1::Snapshot {
                revision,
                selection,
            },
            resolved,
        });
        if target.is_some() {
            *scope = target;
        }
        drop(scope);
        drop(state);
        self.publish();
        Ok(())
    }

    fn expire(&self) {
        let expired = self.state.preview.lock().ok().and_then(|preview| {
            preview
                .as_ref()
                .filter(|preview| std::time::Instant::now() >= preview.expires_at)
                .map(|preview| preview.token)
        });
        if let Some(token) = expired {
            if let Err(error) = self.state.cancel_preview(&self.context, token) {
                tracing::warn!(%error, "view preview expiry requires recovery");
            } else {
                self.publish();
            }
        }
    }

    fn effective(&self) -> Result<ResolvedTheme, String> {
        if self
            .state
            .recovery_required
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err("theme view requires recovery".into());
        }
        let preview = self
            .state
            .preview
            .lock()
            .map_err(|_| "preview requires recovery")?;
        if let Some(preview) = preview.as_ref() {
            return Ok(preview.displayed.clone());
        }
        drop(preview);
        self.state
            .selection
            .lock()
            .map_err(|_| "theme requires recovery")?
            .as_ref()
            .map(|live| live.resolved.clone())
            .ok_or_else(|| "theme not initialized".into())
    }
}

impl RustPlugin for ThemePlugin {
    type Contract = control_contract::Contract;

    fn activate(&mut self, context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        // No lifecycle call applies theme/appearance globally.
        self.watch_detach(&context);
        Ok(EXIT_OK)
    }

    fn handle_event(&mut self, _event: PluginEvent) -> Result<i32, PluginCommandError> {
        Ok(EXIT_OK)
    }

    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        if context.caller_client_id.is_none() {
            return Err(PluginCommandError::unavailable(
                "theme command requires an attachment; use explicit default control for persistence",
            ));
        }
        bmux_plugin_sdk::route_command!(context, {
            "pick-theme" => super::pick_theme(&context),
            "refresh-theme" => super::refresh_theme(&context),
        })
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.service.interface_id == "theme-control-v1"
            && matches!(
                context.request.operation.as_str(),
                "current-default" | "select-default"
            )
        {
            return bmux_plugin_sdk::route_service!(context, {
                "theme-control-v1", "current-default" => |_req: (), ctx| {
                    self.default_snapshot(ctx).map_err(|error| ServiceResponse::error("theme_default_unavailable", error))
                },
                "theme-control-v1", "select-default" => |req: control_contract::theme_control_v1::client::SelectDefaultRequest, ctx| {
                    Ok::<_, ServiceResponse>(self.select_default(ctx, req.expected_revision, req.selection))
                },
            });
        }
        if context.request.service.interface_id == "theme-state"
            && context.caller_client_id.is_none()
        {
            return baseline_appearance(&context);
        }
        let view = match self.view(&context) {
            Ok(view) => view,
            Err(error) => return error,
        };
        let Ok(_operation) = view.operation.lock() else {
            return ServiceResponse::error("theme_recovery_required", "view requires recovery");
        };
        if view.closed.load(std::sync::atomic::Ordering::Acquire) {
            return ServiceResponse::error("attachment_closed", "theme view is closed");
        }
        view.expire();
        let target = match appearance_scope(&context) {
            Ok(target) => target,
            Err(error) => return error,
        };
        if let Err(error) = view.initialize(&context, target) {
            return ServiceResponse::error("theme_not_found", error);
        }
        if context.request.service.interface_id == "theme-state" {
            view.publish();
            return bmux_plugin_sdk::route_service!(context, {
                "theme-state", "active-appearance" => |_req: (), _ctx| {
                    view.effective().map(|theme| control_contract::theme_state::Appearance::from(theme.appearance)).map_err(|error| ServiceResponse::error("theme_not_found", error))
                },
                "theme-state", "active-appearance-for-cwd" => |_req: control_contract::theme_state::client::ActiveAppearanceForCwdRequest, _ctx| {
                    view.effective().map(|theme| control_contract::theme_state::Appearance::from(theme.appearance)).map_err(|error| ServiceResponse::error("theme_not_found", error))
                },
                "theme-state", "active-appearance-for-scope" => |_req: control_contract::theme_state::client::ActiveAppearanceForScopeRequest, _ctx| {
                    view.effective().map(|theme| control_contract::theme_state::Appearance::from(theme.appearance)).map_err(|error| ServiceResponse::error("theme_not_found", error))
                },
            });
        }
        if context.request.operation == "reset-view" {
            return bmux_plugin_sdk::route_service!(context, {
                "theme-control-v1", "reset-view" => |req: control_contract::theme_control_v1::client::ResetViewRequest, ctx| {
                    let result = view.state.current_settings(ctx).and_then(|settings| {
                        let catalog = load_theme_catalog(&ctx.connection.config_dir_candidate_paths());
                        let selection = baseline_selection(ctx, &settings, &catalog)?;
                        view.state.select_live_theme(ctx, req.expected_revision, selection)
                    });
                    if result.is_ok() { view.explicit.store(false, std::sync::atomic::Ordering::Release); view.publish(); }
                    Ok::<_, ServiceResponse>(result)
                },
            });
        }
        let explicit = matches!(
            context.request.operation.as_str(),
            "select" | "confirm-preview" | "set-component-settings"
        );
        let response = view.state.invoke_service(context);
        if explicit && response.error.is_none() {
            // Inner result is checked too: rejected mutations do not pin a baseline.
            if bmux_plugin_sdk::decode_service_message::<
                Result<control_contract::theme_control_v1::Snapshot, String>,
            >(&response.payload)
            .is_ok_and(|result| result.is_ok())
            {
                view.explicit
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        }
        if response.error.is_none() {
            view.publish();
        }
        response
    }
}

// A separate atomic preference distinguishes explicit defaults from legacy picker
// preferences without reinterpreting declared_on_connect or rewriting old data.
fn explicit_default(
    context: &(impl bmux_plugin::HostRuntimeApi + ?Sized),
) -> Result<Option<ThemeSelection>, String> {
    let bytes = context
        .storage_get(&bmux_plugin_sdk::StorageGetRequest::new(
            bmux_plugin_sdk::storage_key!("default_theme"),
        ))
        .map_err(|error| error.to_string())?
        .value;
    bytes
        .map(|bytes| {
            super::decode_theme_selection(&bytes).map(|preset| {
                preset.map_or(ThemeSelection::Configured, |name| ThemeSelection::Preset {
                    name,
                })
            })
        })
        .transpose()
}

#[cfg(test)]
#[path = "views_tests.rs"]
mod tests;

fn baseline_selection(
    context: &NativeServiceContext,
    settings: &super::ThemePluginSettings,
    catalog: &[super::ThemeCatalogEntry],
) -> Result<ThemeSelection, String> {
    if let Some(selection) = explicit_default(context)? {
        return Ok(selection);
    }
    let active = active_theme_stack(context, settings, catalog).ok_or("default theme not found")?;
    if active.source == ActiveThemeSource::Persisted {
        ThemeSelection::from_picker_value(&picker_selection_name(&active))
            .ok_or_else(|| "invalid default".into())
    } else {
        Ok(ThemeSelection::Configured)
    }
}

fn baseline_appearance(context: &NativeServiceContext) -> ServiceResponse {
    bmux_plugin_sdk::route_service!(context, {
        "theme-state", "active-appearance" => |_req: (), ctx| { resolve_baseline_appearance(ctx, None) },
        "theme-state", "active-appearance-for-cwd" => |req: control_contract::theme_state::client::ActiveAppearanceForCwdRequest, ctx| { resolve_baseline_appearance(ctx, Some(ConfigScopeTarget::with_cwd("pane", req.cwd))) },
        "theme-state", "active-appearance-for-scope" => |req: control_contract::theme_state::client::ActiveAppearanceForScopeRequest, ctx| { resolve_baseline_appearance(ctx, Some(ConfigScopeTarget { name: req.scope, attributes: req.attributes })) },
    })
}

fn resolve_baseline_appearance(
    context: &NativeServiceContext,
    target: Option<ConfigScopeTarget>,
) -> Result<control_contract::theme_state::Appearance, ServiceResponse> {
    let resolved = (|| {
        let settings = if let Some(target) = target {
            scoped_theme_settings_for_target(context, target)?
        } else {
            try_parse_settings(context.settings.as_ref())?
        };
        let catalog = load_theme_catalog(&context.connection.config_dir_candidate_paths());
        let selection = baseline_selection(context, &settings, &catalog)?;
        resolve_live_selection(context, &catalog, &selection, &settings)
    })();
    resolved
        .map(|theme| control_contract::theme_state::Appearance::from(theme.appearance))
        .map_err(|error| ServiceResponse::error("theme_not_found", error))
}

impl Drop for ThemePlugin {
    fn drop(&mut self) {
        self.detach_task.take();
        let retired = self
            .views
            .lock()
            .map(|mut views| std::mem::take(&mut *views))
            .unwrap_or_default();
        for view in retired.values() {
            view.close();
        }
    }
}

fn appearance_scope(
    context: &NativeServiceContext,
) -> Result<Option<ConfigScopeTarget>, ServiceResponse> {
    if context.request.service.interface_id != "theme-state" {
        return Ok(None);
    }
    let invalid = |error: bmux_plugin_sdk::PluginError| {
        ServiceResponse::error("invalid_request", error.to_string())
    };
    match context.request.operation.as_str() {
        "active-appearance-for-cwd" => {
            let req: control_contract::theme_state::client::ActiveAppearanceForCwdRequest =
                bmux_plugin_sdk::decode_service_message(&context.request.payload)
                    .map_err(invalid)?;
            Ok(Some(ConfigScopeTarget::with_cwd("pane", req.cwd)))
        }
        "active-appearance-for-scope" => {
            let req: control_contract::theme_state::client::ActiveAppearanceForScopeRequest =
                bmux_plugin_sdk::decode_service_message(&context.request.payload)
                    .map_err(invalid)?;
            Ok(Some(ConfigScopeTarget {
                name: req.scope,
                attributes: req.attributes,
            }))
        }
        _ => Ok(None),
    }
}
