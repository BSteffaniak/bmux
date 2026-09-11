#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

mod storage_migration;

use api_contexts_state::{ContextSelector, ContextSummary};
use bmux_clients_plugin_api::clients_state as api_clients_state;
use bmux_contexts_plugin_api::{contexts_commands, contexts_state as api_contexts_state};
use bmux_pane_runtime_plugin_api::{
    pane_runtime_commands as api_pane_runtime_commands,
    pane_runtime_state as api_pane_runtime_state,
};
use bmux_plugin::{HostRuntimeApi, ServiceCaller, TypedServiceCaller, prompt};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    LogWriteLevel, LogWriteRequest, PromptPolicy, PromptRequest, PromptResponse, PromptValidation,
    PromptValue, StorageGetRequest, StorageSetRequest, TypedServiceRegistrationContext,
    TypedServiceRegistry, VolatileStateClearRequest, VolatileStateGetRequest,
    VolatileStateSetRequest,
    perf_telemetry::{PhaseChannel, PhasePayload, emit as emit_phase_timing},
};
use bmux_tabs_plugin_api::tabs_commands::{
    self, CloseError, FloatingPaneMoveDirection, FocusError, PaneAck, PaneDirection,
    PaneMutationError, PaneResizeDirection, PaneZoomAck, Selector, TabAck, TabError,
    TabMovePlacement, TabsCommandsService,
};
use bmux_tabs_plugin_api::tabs_state::{
    self, ActiveTabPaneQueryError, ActiveTabPaneSet, FloatingPaneState, PaneState, TabEntry,
    TabsStateService,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;

const ACTIVE_TAB_CONTEXT_KEY: &str = "tabs.active_context_id";
const PREVIOUS_TAB_CONTEXT_KEY: &str = "tabs.previous_context_id";
const COMMAND_OUTCOME_SELECTED_CONTEXT_ID_KEY: &str = "bmux.contexts.selected_context_id";

fn storage_key(key: &str) -> bmux_plugin_sdk::StorageKey {
    bmux_plugin_sdk::StorageKey::new(key).expect("tabs plugin storage key should be valid")
}

fn typed_service_error(operation: &'static str, err: impl std::fmt::Display) -> String {
    format!("{operation} failed: {err}")
}

const fn dispatch_client<C: ServiceCaller + Sync + ?Sized>(
    caller: &C,
) -> bmux_plugin::ServiceCallerDispatchClient<'_, C> {
    bmux_plugin::ServiceCallerDispatchClient::new(caller)
}

const fn context_selector_by_id(id: Uuid) -> ContextSelector {
    ContextSelector {
        id: Some(id),
        name: None,
    }
}

struct LaunchPaneRequest {
    direction: PaneDirection,
    name: Option<String>,
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
}

fn selector_matches_context(selector: &ContextSelector, context: &ContextSummary) -> bool {
    if let Some(id) = selector.id {
        return context.id == id;
    }
    selector
        .name
        .as_deref()
        .is_some_and(|name| context.name.as_deref() == Some(name))
}

fn tabs_selector_to_session_selector(
    selector: &Selector,
) -> bmux_sessions_plugin_api::sessions_state::SessionSelector {
    bmux_sessions_plugin_api::sessions_state::SessionSelector {
        id: selector.id,
        name: selector.name.clone(),
    }
}

fn list_contexts(caller: &(impl ServiceCaller + Sync)) -> Result<Vec<ContextSummary>, String> {
    let mut client = dispatch_client(caller);
    bmux_plugin::block_on_typed_dispatch(api_contexts_state::client::list_contexts(&mut client))
        .map_err(|err| typed_service_error("contexts-state/list-contexts", err))
}

fn active_workspace_id(caller: &(impl ServiceCaller + Sync)) -> Uuid {
    let mut client = dispatch_client(caller);
    bmux_plugin::block_on_typed_dispatch(
        bmux_workspaces_plugin_api::workspaces_state::client::current_workspace(&mut client),
    )
    .ok()
    .flatten()
    .map_or_else(Uuid::nil, |workspace| workspace.id)
}

fn filter_contexts_for_workspace(
    contexts: Vec<ContextSummary>,
    workspace_id: Uuid,
) -> Vec<ContextSummary> {
    contexts
        .into_iter()
        .filter(|context| context_workspace_id(context) == workspace_id)
        .collect()
}

fn list_contexts_in_active_workspace(
    caller: &(impl ServiceCaller + Sync),
) -> Result<Vec<ContextSummary>, String> {
    Ok(filter_contexts_for_workspace(
        list_contexts(caller)?,
        active_workspace_id(caller),
    ))
}

fn current_context(caller: &(impl ServiceCaller + Sync)) -> Result<Option<ContextSummary>, String> {
    let mut client = dispatch_client(caller);
    bmux_plugin::block_on_typed_dispatch(api_contexts_state::client::current_context(&mut client))
        .map_err(|err| typed_service_error("contexts-state/current-context", err))
}

fn active_tab_pane_set(
    tab: &ContextSummary,
    selected_session_id: Option<Uuid>,
    panes: api_pane_runtime_state::SessionPaneList,
) -> Result<ActiveTabPaneSet, ActiveTabPaneQueryError> {
    let session_id = selected_session_id.ok_or(ActiveTabPaneQueryError::NoSelectedSession)?;
    if panes.session_id != session_id {
        return Err(ActiveTabPaneQueryError::Failed {
            reason: format!(
                "pane runtime returned session {} while active tab targets {session_id}",
                panes.session_id
            ),
        });
    }
    Ok(ActiveTabPaneSet {
        tab_id: tab.id,
        session_id,
        pane_ids: panes.panes.into_iter().map(|pane| pane.id).collect(),
    })
}

fn active_tab_panes(
    caller: &(impl ServiceCaller + Sync),
) -> Result<ActiveTabPaneSet, ActiveTabPaneQueryError> {
    let tab = current_context(caller)
        .map_err(|reason| ActiveTabPaneQueryError::Failed { reason })?
        .ok_or(ActiveTabPaneQueryError::NoActiveTab)?;
    let mut client = dispatch_client(caller);
    let current_client = bmux_plugin::block_on_typed_dispatch(
        api_clients_state::client::current_client(&mut client),
    )
    .map_err(|error| ActiveTabPaneQueryError::Failed {
        reason: typed_service_error("clients-state/current-client", error),
    })?
    .map_err(|error| ActiveTabPaneQueryError::Failed {
        reason: format!("current client unavailable: {error:?}"),
    })?;
    if current_client
        .selected_context_id
        .is_some_and(|id| id != tab.id)
    {
        return Err(ActiveTabPaneQueryError::Failed {
            reason: "active tab and current client context are temporarily inconsistent"
                .to_string(),
        });
    }
    let session_id = current_client
        .selected_session_id
        .ok_or(ActiveTabPaneQueryError::NoSelectedSession)?;
    let panes = list_panes(caller, Some(session_id))
        .map_err(|reason| ActiveTabPaneQueryError::Failed { reason })?;
    active_tab_pane_set(&tab, Some(session_id), panes)
}

fn create_context(
    caller: &(impl ServiceCaller + Sync),
    name: Option<String>,
    attributes: BTreeMap<String, String>,
) -> Result<ContextSummary, String> {
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(contexts_commands::client::create_context(
        &mut client,
        name.clone(),
        attributes.clone(),
    ))
    .map_err(|err| typed_service_error("contexts-commands/create-context", err))?;
    let ack = result.map_err(|err| format!("create-context failed: {err:?}"))?;
    Ok(ContextSummary {
        id: ack.id,
        name,
        attributes,
    })
}

fn rename_context(
    caller: &(impl ServiceCaller + Sync),
    selector: ContextSelector,
    name: String,
) -> Result<Uuid, String> {
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(contexts_commands::client::rename_context(
        &mut client,
        selector,
        name,
    ))
    .map_err(|err| typed_service_error("contexts-commands/rename-context", err))?;
    result
        .map(|ack| ack.id)
        .map_err(|err| format!("rename-context failed: {err:?}"))
}

fn select_context(
    caller: &(impl ServiceCaller + Sync),
    selector: ContextSelector,
) -> Result<Uuid, String> {
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(contexts_commands::client::select_context(
        &mut client,
        selector,
    ))
    .map_err(|err| typed_service_error("contexts-commands/select-context", err))?;
    result
        .map(|ack| ack.id)
        .map_err(|err| format!("select-context failed: {err:?}"))
}

fn close_context(
    caller: &(impl ServiceCaller + Sync),
    selector: ContextSelector,
    force: bool,
) -> Result<Uuid, String> {
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(contexts_commands::client::close_context(
        &mut client,
        selector,
        force,
    ))
    .map_err(|err| typed_service_error("contexts-commands/close-context", err))?;
    result
        .map(|ack| ack.id)
        .map_err(|err| format!("close-context failed: {err:?}"))
}

fn resolve_session_id(
    caller: &(impl ServiceCaller + Sync),
    selector: Option<&Selector>,
) -> Result<Uuid, String> {
    if let Some(selector) = selector {
        if let Some(id) = selector.id {
            return Ok(id);
        }
        if selector.name.is_some() {
            let mut client = dispatch_client(caller);
            let result = bmux_plugin::block_on_typed_dispatch(
                bmux_sessions_plugin_api::sessions_state::client::get_session(
                    &mut client,
                    tabs_selector_to_session_selector(selector),
                ),
            )
            .map_err(|err| typed_service_error("sessions-state/get-session", err))?;
            return result
                .map(|session| session.id)
                .map_err(|err| format!("session selector did not resolve: {err:?}"));
        }
    }

    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(api_clients_state::client::current_client(
        &mut client,
    ))
    .map_err(|err| typed_service_error("clients-state/current-client", err))?;
    result
        .map_err(|err| format!("current client unavailable: {err:?}"))?
        .selected_session_id
        .ok_or_else(|| "current client has no selected session".to_string())
}

fn list_panes(
    caller: &(impl ServiceCaller + Sync),
    session_id: Option<Uuid>,
) -> Result<api_pane_runtime_state::SessionPaneList, String> {
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(api_pane_runtime_state::client::list_panes(
        &mut client,
        session_id,
    ))
    .map_err(|err| typed_service_error("pane-runtime-state/list-panes", err))?;
    result.map_err(|err| format!("list-panes failed: {err:?}"))
}

fn list_floating_panes(
    caller: &(impl ServiceCaller + Sync),
    session_id: Option<Uuid>,
) -> Result<Vec<(Uuid, api_pane_runtime_state::FloatingPaneSummary)>, String> {
    let session_ids = if let Some(session_id) = session_id {
        vec![session_id]
    } else {
        list_contexts(caller)?
            .into_iter()
            .map(|context| context.id)
            .collect()
    };
    let mut panes = Vec::new();
    for session_id in session_ids {
        let mut client = dispatch_client(caller);
        let result = bmux_plugin::block_on_typed_dispatch(
            api_pane_runtime_state::client::list_floating_panes(&mut client, session_id),
        )
        .map_err(|err| typed_service_error("pane-runtime-state/list-floating-panes", err))?;
        if let Ok(list) = result {
            panes.extend(list.panes.into_iter().map(|pane| (session_id, pane)));
        }
    }
    Ok(panes)
}

fn resolve_target_pane_id(
    caller: &(impl ServiceCaller + Sync),
    session_id: Uuid,
    selector: Option<&Selector>,
) -> Result<Option<Uuid>, String> {
    let Some(selector) = selector else {
        return Ok(None);
    };
    if let Some(id) = selector.id {
        return Ok(Some(id));
    }
    let Some(index) = selector.index else {
        return Ok(None);
    };
    let panes = list_panes(caller, Some(session_id))?.panes;
    panes
        .into_iter()
        .enumerate()
        .find(|(idx, _)| u32::try_from(*idx).ok() == Some(index))
        .map(|(_, pane)| Some(pane.id))
        .ok_or_else(|| format!("pane index '{index}' not found"))
}

fn focus_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
    target: Option<&Selector>,
    direction: &str,
) -> Result<api_pane_runtime_commands::PaneAck, String> {
    let session_id = resolve_session_id(caller, session)?;
    let target = resolve_target_pane_id(caller, session_id, target)?;
    let mut client = dispatch_client(caller);
    let result =
        bmux_plugin::block_on_typed_dispatch(api_pane_runtime_commands::client::focus_pane(
            &mut client,
            session_id,
            target,
            direction.to_string(),
        ))
        .map_err(|err| typed_service_error("pane-runtime-commands/focus-pane", err))?;
    let ack = result.map_err(|err| format!("focus-pane failed: {err:?}"))?;
    emit_pane_event(bmux_tabs_plugin_api::tabs_events::PaneEvent::Focused {
        pane_id: ack.pane_id,
    });
    Ok(ack)
}

fn split_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
    target: Option<&Selector>,
    direction: PaneDirection,
    ratio_pct: Option<u32>,
) -> Result<api_pane_runtime_commands::PaneAck, String> {
    let session_id = resolve_session_id(caller, session)?;
    let target = resolve_target_pane_id(caller, session_id, target)?;
    let ratio = ratio_pct.unwrap_or(50).clamp(10, 90) as u8;
    let mut client = dispatch_client(caller);
    let result =
        bmux_plugin::block_on_typed_dispatch(api_pane_runtime_commands::client::split_pane(
            &mut client,
            session_id,
            target,
            pane_direction_name(direction).to_string(),
            ratio,
        ))
        .map_err(|err| typed_service_error("pane-runtime-commands/split-pane", err))?;
    let ack = result.map_err(|err| format!("split-pane failed: {err:?}"))?;
    emit_pane_event(bmux_tabs_plugin_api::tabs_events::PaneEvent::Opened {
        pane_id: ack.pane_id,
        session_id: ack.session_id,
    });
    Ok(ack)
}

fn launch_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
    target: Option<&Selector>,
    request: LaunchPaneRequest,
) -> Result<api_pane_runtime_commands::PaneAck, String> {
    let session_id = resolve_session_id(caller, session)?;
    let target = resolve_target_pane_id(caller, session_id, target)?;
    let mut client = dispatch_client(caller);
    let result =
        bmux_plugin::block_on_typed_dispatch(api_pane_runtime_commands::client::launch_pane(
            &mut client,
            session_id,
            target,
            pane_direction_name(request.direction).to_string(),
            50,
            request.name,
            request.program,
            request.args,
            request.cwd,
        ))
        .map_err(|err| typed_service_error("pane-runtime-commands/launch-pane", err))?;
    let ack = result.map_err(|err| format!("launch-pane failed: {err:?}"))?;
    emit_pane_event(bmux_tabs_plugin_api::tabs_events::PaneEvent::Opened {
        pane_id: ack.pane_id,
        session_id: ack.session_id,
    });
    Ok(ack)
}

fn resize_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
    target: Option<&Selector>,
    direction: PaneResizeDirection,
    cells: u16,
) -> Result<api_pane_runtime_commands::SessionAck, String> {
    let session_id = resolve_session_id(caller, session)?;
    let target = resolve_target_pane_id(caller, session_id, target)?;
    let mut client = dispatch_client(caller);
    let result =
        bmux_plugin::block_on_typed_dispatch(api_pane_runtime_commands::client::resize_pane(
            &mut client,
            session_id,
            target,
            resize_direction_name(direction).to_string(),
            cells.max(1),
        ))
        .map_err(|err| typed_service_error("pane-runtime-commands/resize-pane", err))?;
    result.map_err(|err| format!("resize-pane failed: {err:?}"))
}

fn close_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
    target: Option<&Selector>,
) -> Result<api_pane_runtime_commands::PaneAck, String> {
    let session_id = resolve_session_id(caller, session)?;
    let target = resolve_target_pane_id(caller, session_id, target)?;
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(
        api_pane_runtime_commands::client::close_pane(&mut client, session_id, target),
    )
    .map_err(|err| typed_service_error("pane-runtime-commands/close-pane", err))?;
    let ack = result.map_err(|err| format!("close-pane failed: {err:?}"))?;
    emit_pane_event(bmux_tabs_plugin_api::tabs_events::PaneEvent::Closed {
        pane_id: ack.pane_id,
    });
    Ok(ack)
}

/// Kills whatever is running in the target pane and respawns a fresh
/// shell in the same layout slot. The pane-runtime primitive preserves
/// the pane id, name, and last-known cwd while dropping the recorded
/// active command, so the pane returns to a clean prompt without
/// disturbing the layout tree.
fn restart_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
    target: Option<&Selector>,
) -> Result<api_pane_runtime_commands::PaneAck, String> {
    let session_id = resolve_session_id(caller, session)?;
    let target = resolve_target_pane_id(caller, session_id, target)?;
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(
        api_pane_runtime_commands::client::restart_pane(&mut client, session_id, target),
    )
    .map_err(|err| typed_service_error("pane-runtime-commands/restart-pane", err))?;
    let ack = result.map_err(|err| format!("restart-pane failed: {err:?}"))?;
    // The tabs-plugin pane-event variant set has no dedicated
    // `restarted` case; a respawned pane is a lifecycle transition back
    // to running, which is exactly what `status-changed` models.
    emit_pane_event(
        bmux_tabs_plugin_api::tabs_events::PaneEvent::StatusChanged {
            pane_id: ack.pane_id,
        },
    );
    Ok(ack)
}

#[derive(Default)]
struct FloatingPaneCommandOptions {
    origin_x: Option<u16>,
    origin_y: Option<u16>,
    width: Option<u16>,
    height: Option<u16>,
    z_index: Option<i32>,
    layer: Option<String>,
    scope: Option<String>,
    program: Option<String>,
    args: Vec<String>,
}

impl FloatingPaneCommandOptions {
    fn overlay_cli_arguments(&mut self, arguments: &[String]) -> Result<(), String> {
        if let Some(value) = option_value(arguments, "x") {
            self.origin_x = Some(parse_u16_arg(&value, "x")?);
        }
        if let Some(value) = option_value(arguments, "y") {
            self.origin_y = Some(parse_u16_arg(&value, "y")?);
        }
        if let Some(value) = option_value(arguments, "w") {
            self.width = Some(parse_u16_arg(&value, "w")?);
        }
        if let Some(value) = option_value(arguments, "h") {
            self.height = Some(parse_u16_arg(&value, "h")?);
        }
        if let Some(value) = option_value(arguments, "z") {
            self.z_index = Some(parse_i32_arg(&value, "z")?);
        }
        if let Some(value) = option_value(arguments, "layer") {
            self.layer = Some(value);
        }
        if let Some(value) = option_value(arguments, "scope") {
            self.scope = Some(value);
        }
        if let Some(value) = option_value(arguments, "program") {
            self.program = Some(value);
        }
        Ok(())
    }
}

fn floating_pane_defaults(
    settings: Option<&toml::Value>,
) -> Result<FloatingPaneCommandOptions, String> {
    let Some(section) = floating_pane_defaults_section(settings) else {
        return Ok(FloatingPaneCommandOptions::default());
    };
    Ok(FloatingPaneCommandOptions {
        origin_x: toml_u16_field(section, "x")?,
        origin_y: toml_u16_field(section, "y")?,
        width: toml_u16_field(section, "w")?,
        height: toml_u16_field(section, "h")?,
        z_index: toml_i32_field(section, "z")?,
        layer: toml_string_field(section, "layer")?,
        scope: toml_string_field(section, "scope")?,
        program: toml_string_field(section, "program")?,
        args: toml_string_list_field(section, "args")?.unwrap_or_default(),
    })
}

fn floating_pane_defaults_section(settings: Option<&toml::Value>) -> Option<&toml::Value> {
    let settings = settings?;
    let floating = settings
        .get("floating_pane")
        .or_else(|| settings.get("floating-pane"))?;
    floating.get("defaults").or(Some(floating))
}

fn toml_u16_field(section: &toml::Value, key: &str) -> Result<Option<u16>, String> {
    let Some(value) = section.get(key) else {
        return Ok(None);
    };
    match value {
        toml::Value::Integer(raw) => u16::try_from(*raw)
            .map(Some)
            .map_err(|_| format!("invalid floating_pane.{key}: expected u16")),
        toml::Value::String(raw) => parse_u16_arg(raw, key).map(Some),
        _ => Err(format!("invalid floating_pane.{key}: expected integer")),
    }
}

fn toml_i32_field(section: &toml::Value, key: &str) -> Result<Option<i32>, String> {
    let Some(value) = section.get(key) else {
        return Ok(None);
    };
    match value {
        toml::Value::Integer(raw) => i32::try_from(*raw)
            .map(Some)
            .map_err(|_| format!("invalid floating_pane.{key}: expected i32")),
        toml::Value::String(raw) => parse_i32_arg(raw, key).map(Some),
        _ => Err(format!("invalid floating_pane.{key}: expected integer")),
    }
}

fn toml_string_field(section: &toml::Value, key: &str) -> Result<Option<String>, String> {
    let Some(value) = section.get(key) else {
        return Ok(None);
    };
    match value {
        toml::Value::String(raw) => Ok(Some(raw.clone())),
        _ => Err(format!("invalid floating_pane.{key}: expected string")),
    }
}

fn toml_string_list_field(section: &toml::Value, key: &str) -> Result<Option<Vec<String>>, String> {
    let Some(value) = section.get(key) else {
        return Ok(None);
    };
    match value {
        toml::Value::Array(values) => values
            .iter()
            .map(|value| match value {
                toml::Value::String(raw) => Ok(raw.clone()),
                _ => Err(format!(
                    "invalid floating_pane.{key}: expected string array"
                )),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        _ => Err(format!(
            "invalid floating_pane.{key}: expected string array"
        )),
    }
}

fn create_floating_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
    target: Option<&Selector>,
    options: FloatingPaneCommandOptions,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    let session_id = resolve_session_id(caller, session)?;
    let target = resolve_target_pane_id(caller, session_id, target)?;
    let context_id = current_context(caller)
        .ok()
        .flatten()
        .map(|context| context.id);
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(
        api_pane_runtime_commands::client::create_floating_pane(
            &mut client,
            session_id,
            target,
            target,
            context_id,
            None,
            options.origin_x,
            options.origin_y,
            options.width,
            options.height,
            options.z_index,
            options.layer,
            options.scope,
            None,
            options.program,
            options.args,
            None,
        ),
    )
    .map_err(|err| typed_service_error("pane-runtime-commands/create-floating-pane", err))?;
    result.map_err(|err| format!("create-floating-pane failed: {err:?}"))
}

fn create_floating_pane_command(
    caller: &(impl ServiceCaller + Sync),
    options: FloatingPaneCommandOptions,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    create_floating_pane(caller, None, None, options)
}

fn mutate_floating_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
    target: &Selector,
    command: &str,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    let session_id = resolve_session_id(caller, session)?;
    let pane_id = resolve_target_pane_id(caller, session_id, Some(target))?
        .ok_or_else(|| "floating pane target did not resolve".to_string())?;
    let mut client = dispatch_client(caller);
    let result = match command {
        "focus" => bmux_plugin::block_on_typed_dispatch(
            api_pane_runtime_commands::client::focus_floating_pane(
                &mut client,
                session_id,
                pane_id,
            ),
        ),
        "raise" => bmux_plugin::block_on_typed_dispatch(
            api_pane_runtime_commands::client::raise_floating_pane(
                &mut client,
                session_id,
                pane_id,
            ),
        ),
        "lower" => bmux_plugin::block_on_typed_dispatch(
            api_pane_runtime_commands::client::lower_floating_pane(
                &mut client,
                session_id,
                pane_id,
            ),
        ),
        "close" => bmux_plugin::block_on_typed_dispatch(
            api_pane_runtime_commands::client::close_floating_pane(
                &mut client,
                session_id,
                pane_id,
            ),
        ),
        other => return Err(format!("unknown floating pane command '{other}'")),
    }
    .map_err(|err| typed_service_error("pane-runtime-commands/floating-pane", err))?;
    result.map_err(|err| format!("{command}-floating-pane failed: {err:?}"))
}

fn mutate_floating_pane_command(
    caller: &(impl ServiceCaller + Sync),
    command: &str,
    pane_id: Uuid,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    mutate_floating_pane(
        caller,
        None,
        &Selector {
            id: Some(pane_id),
            name: None,
            index: None,
        },
        command,
    )
}

fn move_floating_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
    target: &Selector,
    origin_x: u16,
    origin_y: u16,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    let session_id = resolve_session_id(caller, session)?;
    let pane_id = resolve_target_pane_id(caller, session_id, Some(target))?
        .ok_or_else(|| "floating pane target did not resolve".to_string())?;
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(
        api_pane_runtime_commands::client::move_floating_pane(
            &mut client,
            session_id,
            pane_id,
            origin_x,
            origin_y,
        ),
    )
    .map_err(|err| typed_service_error("pane-runtime-commands/move-floating-pane", err))?;
    result.map_err(|err| format!("move-floating-pane failed: {err:?}"))
}

fn move_floating_pane_command(
    caller: &(impl ServiceCaller + Sync),
    pane_id: Uuid,
    origin_x: u16,
    origin_y: u16,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    move_floating_pane(
        caller,
        None,
        &Selector {
            id: Some(pane_id),
            name: None,
            index: None,
        },
        origin_x,
        origin_y,
    )
}

fn resize_floating_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
    target: &Selector,
    width: u16,
    height: u16,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    let session_id = resolve_session_id(caller, session)?;
    let pane_id = resolve_target_pane_id(caller, session_id, Some(target))?
        .ok_or_else(|| "floating pane target did not resolve".to_string())?;
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(
        api_pane_runtime_commands::client::resize_floating_pane(
            &mut client,
            session_id,
            pane_id,
            width,
            height,
        ),
    )
    .map_err(|err| typed_service_error("pane-runtime-commands/resize-floating-pane", err))?;
    result.map_err(|err| format!("resize-floating-pane failed: {err:?}"))
}

fn resize_floating_pane_command(
    caller: &(impl ServiceCaller + Sync),
    pane_id: Uuid,
    width: u16,
    height: u16,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    resize_floating_pane(
        caller,
        None,
        &Selector {
            id: Some(pane_id),
            name: None,
            index: None,
        },
        width,
        height,
    )
}

const fn selector_by_id(id: Uuid) -> Selector {
    Selector {
        id: Some(id),
        name: None,
        index: None,
    }
}

fn active_floating_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
) -> Result<(Selector, api_pane_runtime_state::FloatingPaneSummary), String> {
    let requested_session_id = session
        .map(|selector| resolve_session_id(caller, Some(selector)))
        .transpose()?;
    let mut panes = list_floating_panes(caller, requested_session_id)?;
    panes.sort_by_key(|(session_id, pane)| (*session_id, pane.z, pane.pane_id));
    let (session_id, pane) = panes
        .iter()
        .find(|(_, pane)| pane.cursor_owner)
        .or_else(|| panes.iter().find(|(_, pane)| pane.visible))
        .ok_or_else(|| "no floating panes found".to_string())?;
    Ok((selector_by_id(*session_id), pane.clone()))
}

fn active_floating_pane_target(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
) -> Result<(Selector, Selector), String> {
    let (session_id, pane) = active_floating_pane(caller, session)?;
    Ok((session_id, selector_by_id(pane.pane_id)))
}

fn next_floating_pane_target(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
) -> Result<(Selector, Selector), String> {
    let requested_session_id = session
        .map(|selector| resolve_session_id(caller, Some(selector)))
        .transpose()?;
    let mut panes = list_floating_panes(caller, requested_session_id)?;
    panes.retain(|(_, pane)| pane.visible);
    panes.sort_by_key(|(session_id, pane)| (*session_id, pane.z, pane.pane_id));
    if panes.is_empty() {
        return Err("no floating panes found".to_string());
    }
    let current_index = panes
        .iter()
        .position(|(_, pane)| pane.cursor_owner)
        .unwrap_or_else(|| panes.len().saturating_sub(1));
    let (session_id, pane) = &panes[(current_index + 1) % panes.len()];
    Ok((
        Selector {
            id: Some(*session_id),
            name: None,
            index: None,
        },
        Selector {
            id: Some(pane.pane_id),
            name: None,
            index: None,
        },
    ))
}

fn mutate_active_floating_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
    command: &str,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    let (owner_session, target) = active_floating_pane_target(caller, session)?;
    mutate_floating_pane(caller, Some(&owner_session), &target, command)
}

fn focus_next_floating_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    let (owner_session, target) = next_floating_pane_target(caller, session)?;
    mutate_floating_pane(caller, Some(&owner_session), &target, "focus")
}

fn move_active_floating_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
    direction: FloatingPaneMoveDirection,
    cells: u16,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    let (owner_session, pane) = active_floating_pane(caller, session)?;
    let cells = cells.max(1);
    let (x, y) = match direction {
        FloatingPaneMoveDirection::Left => (pane.x.saturating_sub(cells), pane.y),
        FloatingPaneMoveDirection::Right => (pane.x.saturating_add(cells), pane.y),
        FloatingPaneMoveDirection::Up => (pane.x, pane.y.saturating_sub(cells)),
        FloatingPaneMoveDirection::Down => (pane.x, pane.y.saturating_add(cells)),
    };
    move_floating_pane(
        caller,
        Some(&owner_session),
        &selector_by_id(pane.pane_id),
        x,
        y,
    )
}

fn resize_active_floating_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
    direction: PaneResizeDirection,
    cells: u16,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    let (owner_session, pane) = active_floating_pane(caller, session)?;
    let target = selector_by_id(pane.pane_id);
    let cells = cells.max(1);
    let (x, y, w, h) = floating_resize_geometry(&pane, direction, cells);
    if x != pane.x || y != pane.y {
        move_floating_pane(caller, Some(&owner_session), &target, x, y)?;
    }
    resize_floating_pane(caller, Some(&owner_session), &target, w, h)
}

fn floating_resize_geometry(
    pane: &api_pane_runtime_state::FloatingPaneSummary,
    direction: PaneResizeDirection,
    cells: u16,
) -> (u16, u16, u16, u16) {
    match direction {
        PaneResizeDirection::Increase => (
            pane.x,
            pane.y,
            pane.w.saturating_add(cells),
            pane.h.saturating_add(cells),
        ),
        PaneResizeDirection::Decrease => (
            pane.x,
            pane.y,
            pane.w.saturating_sub(cells).max(1),
            pane.h.saturating_sub(cells).max(1),
        ),
        PaneResizeDirection::Left => {
            let x = pane.x.saturating_sub(cells);
            let delta = pane.x.saturating_sub(x);
            (x, pane.y, pane.w.saturating_add(delta), pane.h)
        }
        PaneResizeDirection::Right => (pane.x, pane.y, pane.w.saturating_add(cells), pane.h),
        PaneResizeDirection::Up => {
            let y = pane.y.saturating_sub(cells);
            let delta = pane.y.saturating_sub(y);
            (pane.x, y, pane.w, pane.h.saturating_add(delta))
        }
        PaneResizeDirection::Down => (pane.x, pane.y, pane.w, pane.h.saturating_add(cells)),
    }
}

fn set_floating_pane_z(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
    target: &Selector,
    z: i32,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    let session_id = resolve_session_id(caller, session)?;
    let pane_id = resolve_target_pane_id(caller, session_id, Some(target))?
        .ok_or_else(|| "floating pane target did not resolve".to_string())?;
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(
        api_pane_runtime_commands::client::set_floating_pane_z(&mut client, session_id, pane_id, z),
    )
    .map_err(|err| typed_service_error("pane-runtime-commands/set-floating-pane-z", err))?;
    result.map_err(|err| format!("set-floating-pane-z failed: {err:?}"))
}

fn set_floating_pane_z_command(
    caller: &(impl ServiceCaller + Sync),
    pane_id: Uuid,
    z: i32,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    set_floating_pane_z(
        caller,
        None,
        &Selector {
            id: Some(pane_id),
            name: None,
            index: None,
        },
        z,
    )
}

fn set_floating_pane_layer(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
    target: &Selector,
    layer: String,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    let session_id = resolve_session_id(caller, session)?;
    let pane_id = resolve_target_pane_id(caller, session_id, Some(target))?
        .ok_or_else(|| "floating pane target did not resolve".to_string())?;
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(
        api_pane_runtime_commands::client::set_floating_pane_layer(
            &mut client,
            session_id,
            pane_id,
            layer,
        ),
    )
    .map_err(|err| typed_service_error("pane-runtime-commands/set-floating-pane-layer", err))?;
    result.map_err(|err| format!("set-floating-pane-layer failed: {err:?}"))
}

fn set_floating_pane_layer_command(
    caller: &(impl ServiceCaller + Sync),
    pane_id: Uuid,
    layer: String,
) -> Result<api_pane_runtime_commands::FloatingPaneAck, String> {
    set_floating_pane_layer(
        caller,
        None,
        &Selector {
            id: Some(pane_id),
            name: None,
            index: None,
        },
        layer,
    )
}

fn zoom_pane(
    caller: &(impl ServiceCaller + Sync),
    session: Option<&Selector>,
) -> Result<api_pane_runtime_commands::PaneAck, String> {
    let session_id = resolve_session_id(caller, session)?;
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(
        api_pane_runtime_commands::client::zoom_pane(&mut client, session_id),
    )
    .map_err(|err| typed_service_error("pane-runtime-commands/zoom-pane", err))?;
    let ack = result.map_err(|err| format!("zoom-pane failed: {err:?}"))?;
    emit_pane_event(bmux_tabs_plugin_api::tabs_events::PaneEvent::Zoomed {
        pane_id: ack.pane_id,
    });
    Ok(ack)
}

const fn pane_direction_name(direction: PaneDirection) -> &'static str {
    match direction {
        PaneDirection::Horizontal | PaneDirection::Left | PaneDirection::Right => "horizontal",
        PaneDirection::Vertical | PaneDirection::Up | PaneDirection::Down => "vertical",
    }
}

const fn focus_direction_name(direction: PaneDirection) -> Option<&'static str> {
    match direction {
        PaneDirection::Horizontal | PaneDirection::Vertical => None,
        // The pane-runtime focus primitive is currently ordered-cycle based
        // (`next`/`prev`). Preserve directional keybindings by folding
        // spatial directions onto that stable primitive until pane geometry
        // selection moves behind the typed tabs facade.
        PaneDirection::Left | PaneDirection::Up => Some("prev"),
        PaneDirection::Right | PaneDirection::Down => Some("next"),
    }
}

const fn resize_direction_name(direction: PaneResizeDirection) -> &'static str {
    match direction {
        PaneResizeDirection::Increase => "increase",
        PaneResizeDirection::Decrease => "decrease",
        PaneResizeDirection::Left => "left",
        PaneResizeDirection::Right => "right",
        PaneResizeDirection::Up => "up",
        PaneResizeDirection::Down => "down",
    }
}

fn emit_attach_phase_timing(payload: &serde_json::Value) {
    emit_phase_timing(PhaseChannel::Attach, payload);
}

fn emit_tabs_plugin_phase_timing(payload: &serde_json::Value) {
    emit_phase_timing(PhaseChannel::Plugin, payload);
}

fn emit_pane_event(event: bmux_tabs_plugin_api::tabs_events::PaneEvent) {
    let _ =
        bmux_plugin::global_event_bus().emit(&bmux_tabs_plugin_api::tabs_events::EVENT_KIND, event);
}

/// Shared "last selected pane per client" map. Mutated by the
/// byte-encoded `switch-tab` handler (via the plugin's mutable
/// access in `invoke_service`) AND by the typed
/// [`TabsCommandsService::switch_tab`] impl (via a clone of the
/// same [`Arc<Mutex<_>>`]). Both paths observe the same state.
type LastSelectedByClient = Arc<Mutex<BTreeMap<Uuid, Uuid>>>;

#[derive(Debug, Default)]
struct TabRuntimeState {
    active_context_id: Option<Uuid>,
    previous_context_id: Option<Uuid>,
    tab_order_ids: Option<Vec<Uuid>>,
    tab_order_dirty: bool,
    known_contexts: BTreeMap<Uuid, Option<String>>,
}

type TabRuntimeStateHandle = Arc<Mutex<TabRuntimeState>>;

#[derive(Default)]
pub struct TabsPlugin {
    last_selected_by_client: LastSelectedByClient,
    runtime_state: TabRuntimeStateHandle,
}

impl RustPlugin for TabsPlugin {
    type Contract = bmux_tabs_plugin_api::Contract;

    fn activate(&mut self, context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        storage_migration::migrate(std::path::Path::new(&context.connection.data_dir))
            .map_err(PluginCommandError::failed)?;
        // Register the typed event-bus channel for pane-event so
        // subscribers (decoration, future UI plugins) can wait on
        // `global_event_bus().subscribe::<PaneEvent>(...)` without
        // racing the first emit. Failure to register is non-fatal —
        // the channel may already exist from a prior load.
        let _ = bmux_plugin::global_event_bus()
            .register_channel::<bmux_tabs_plugin_api::tabs_events::PaneEvent>(
                bmux_tabs_plugin_api::tabs_events::EVENT_KIND,
            );

        // Register the reactive state channel carrying the ordered
        // tab list. The attach tab bar subscribes via
        // `subscribe_state::<TabListSnapshot>` and observes every
        // order mutation without polling. Seed with an empty snapshot
        // — the first real publish happens on the first mutation
        // (new-tab / switch-tab / …). If a consumer activates
        // before the first mutation they see an empty list, which
        // correctly reflects that no tabs exist yet.
        bmux_plugin::global_event_bus()
            .register_state_channel::<bmux_tabs_plugin_api::tabs_list::TabListSnapshot>(
                bmux_tabs_plugin_api::tabs_list::STATE_KIND,
                bmux_tabs_plugin_api::tabs_list::TabListSnapshot {
                    tabs: Vec::new(),
                    revision: 0,
                },
            );
        Ok(EXIT_OK)
    }

    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        handle_command(self, &context)?;
        Ok(EXIT_OK)
    }

    #[allow(clippy::too_many_lines)] // route_service! covers every tabs-commands op; the block is naturally long.
    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "tabs-state", "list-tabs" => |req: ListTabsArgs, ctx| {
                let tabs = list_tabs(ctx, &self.runtime_state, req.session.as_deref())
                    .map_err(|e| ServiceResponse::error("list_failed", e))?;
                Ok(tabs)
            },
            "tabs-state", "active-tab-panes" => |_req: (), ctx| {
                Ok::<_, ServiceResponse>(active_tab_panes(ctx))
            },
            "tabs-commands", "new-tab" => |req: NewTabArgs, ctx| {
                Ok::<_, ServiceResponse>(create_tab(ctx, &self.runtime_state, req.name)
                    .map_err(|reason| TabError::Failed { reason }))
            },
            "tabs-commands", "rename-tab" => |req: RenameTabArgs, ctx| {
                Ok::<_, ServiceResponse>(rename_tab(ctx, &self.runtime_state, &req.name)
                    .map_err(|reason| TabError::Failed { reason }))
            },
            "tabs-commands", "rename-tab-by-id" => |req: RenameTabByIdArgs, ctx| {
                Ok::<_, ServiceResponse>(rename_tab_by_id(ctx, &self.runtime_state, req.id, &req.name)
                    .map_err(|reason| TabError::Failed { reason }))
            },
            "tabs-commands", "kill-tab" => |req: KillTabArgs, ctx| {
                let result = parse_selector(&req.target)
                    .and_then(|selector| kill_tab(ctx, &self.runtime_state, selector, req.force_local))
                    .map_err(|reason| TabError::Failed { reason });
                Ok::<_, ServiceResponse>(result)
            },
            "tabs-commands", "kill-all-tabs" => |req: KillAllTabsArgs, ctx| {
                Ok::<_, ServiceResponse>(kill_all_tabs(ctx, &self.runtime_state, req.force_local)
                    .map_err(|reason| TabError::Failed { reason }))
            },
            "tabs-commands", "switch-tab" => |req: SwitchTabArgs, ctx| {
                let result = parse_selector(&req.target).and_then(|selector| {
                    switch_tab(
                        ctx,
                        &self.runtime_state,
                        selector,
                        &self.last_selected_by_client,
                        ctx.caller_client_id,
                    )
                }).map_err(|reason| TabError::Failed { reason });
                Ok::<_, ServiceResponse>(result)
            },
            "tabs-commands", "move-tab" => |req: MoveTabArgs, ctx| {
                Ok::<_, ServiceResponse>(move_tab(ctx, &self.runtime_state, req.source, req.target, req.placement)
                    .map_err(|reason| TabError::Failed { reason }))
            },
            "tabs-commands", "focus-pane" => |req: FocusPaneArgs, ctx| {
                let target = Selector { id: Some(req.id), name: None, index: None };
                focus_pane(ctx, None, Some(&target), "")
                    .map(|ack| PaneAck { ok: true, pane_id: Some(ack.pane_id) })
                    .map_err(|e| ServiceResponse::error("focus_failed", e))
            },
            "tabs-commands", "close-pane" => |req: ClosePaneArgs, ctx| {
                let target = Selector { id: Some(req.id), name: None, index: None };
                close_pane(ctx, None, Some(&target))
                    .map(|ack| PaneAck { ok: true, pane_id: Some(ack.pane_id) })
                    .map_err(|e| ServiceResponse::error("close_failed", e))
            },
            "tabs-commands", "focus-pane-by-selector" => |req: FocusPaneBySelectorArgs, ctx| {
                focus_pane(ctx, req.session.as_ref(), Some(&req.target), "")
                    .map(|ack| PaneAck { ok: true, pane_id: Some(ack.pane_id) })
                    .map_err(|e| ServiceResponse::error("focus_failed", e))
            },
            "tabs-commands", "close-pane-by-selector" => |req: ClosePaneBySelectorArgs, ctx| {
                close_pane(ctx, req.session.as_ref(), Some(&req.target))
                    .map(|ack| PaneAck { ok: true, pane_id: Some(ack.pane_id) })
                    .map_err(|e| ServiceResponse::error("close_failed", e))
            },
            "tabs-commands", "close-active-pane" => |req: CloseActivePaneArgs, ctx| {
                close_pane(ctx, req.session.as_ref(), None)
                    .map(|ack| PaneAck { ok: true, pane_id: Some(ack.pane_id) })
                    .map_err(|e| ServiceResponse::error("close_failed", e))
            },
            "tabs-commands", "focus-pane-in-direction" => |req: FocusPaneInDirectionArgs, ctx| {
                let Some(focus_dir) = focus_direction_name(req.direction) else {
                    return Err(ServiceResponse::error(
                        "invalid_request",
                        "direction must be left/right/up/down",
                    ));
                };
                focus_pane(ctx, req.session.as_ref(), None, focus_dir)
                    .map(|ack| PaneAck { ok: true, pane_id: Some(ack.pane_id) })
                    .map_err(|e| ServiceResponse::error("focus_failed", e))
            },
            "tabs-commands", "split-pane" => |req: SplitPaneArgs, ctx| {
                split_pane(ctx, req.session.as_ref(), req.target.as_ref(), req.direction, req.ratio_pct)
                    .map(|ack| PaneAck { ok: true, pane_id: Some(ack.pane_id) })
                    .map_err(|e| ServiceResponse::error("split_failed", e))
            },
            "tabs-commands", "launch-pane" => |req: LaunchPaneArgs, ctx| {
                launch_pane(ctx, req.session.as_ref(), req.target.as_ref(), LaunchPaneRequest {
                    direction: req.direction,
                    name: req.name,
                    program: req.program,
                    args: req.args,
                    cwd: None,
                })
                    .map(|ack| PaneAck { ok: true, pane_id: Some(ack.pane_id) })
                    .map_err(|e| ServiceResponse::error("launch_failed", e))
            },
            "tabs-commands", "resize-pane" => |req: ResizePaneArgs, ctx| {
                resize_pane(ctx, req.session.as_ref(), req.target.as_ref(), req.direction, req.cells)
                    .map(|_| PaneAck { ok: true, pane_id: None })
                    .map_err(|e| ServiceResponse::error("resize_failed", e))
            },
            "tabs-commands", "move-floating-pane" => |req: MoveFloatingPaneArgs, ctx| {
                move_floating_pane(ctx, req.session.as_ref(), &req.target, req.x, req.y)
                    .map(|ack| PaneAck { ok: true, pane_id: Some(ack.pane_id) })
                    .map_err(|e| ServiceResponse::error("move_floating_failed", e))
            },
            "tabs-commands", "zoom-pane" => |req: ZoomPaneArgs, ctx| {
                zoom_pane(ctx, req.session.as_ref())
                    .map(|ack| PaneZoomAck { pane_id: ack.pane_id, zoomed: true })
                    .map_err(|e| ServiceResponse::error("zoom_failed", e))
            },
            "tabs-commands", "restart-pane" => |req: RestartPaneArgs, ctx| {
                restart_pane(ctx, req.session.as_ref(), req.target.as_ref())
                    .map(|ack| PaneAck { ok: true, pane_id: Some(ack.pane_id) })
                    .map_err(|e| ServiceResponse::error("restart_failed", e))
            },
        })
    }

    fn activate_with_async(
        &mut self,
        context: NativeLifecycleContext,
        async_handle: bmux_plugin_sdk::HostAsyncHandle,
    ) -> Result<i32, PluginCommandError> {
        let result = self.activate(context.clone())?;
        let shared = TabsSharedState {
            caller: Arc::new(TypedServiceCaller::from_lifecycle_context(&context)),
            last_selected_by_client: self.last_selected_by_client.clone(),
            runtime_state: self.runtime_state.clone(),
        };
        let worker = async_handle.clone();
        async_handle.spawn_background("tabs-catalog-observer", move |mut cancellation| async move {
            let bus = bmux_plugin::global_event_bus();
            let mut contexts = None;
            let mut workspaces = None;
            loop {
                use bmux_contexts_plugin_api::contexts_events::{self, ContextEvent};
                use bmux_workspaces_plugin_api::workspaces_events::{self, WorkspaceEvent};
                let contexts_kind = contexts_events::EVENT_KIND;
                let workspaces_kind = workspaces_events::EVENT_KIND;
                let event = tokio::select! {
                    () = cancellation.cancelled() => return Ok(()),
                    ready = bus.subscribe_when_registered::<ContextEvent>(&contexts_kind), if contexts.is_none() => {
                        contexts = Some(ready.map_err(|error| error.to_string())?);
                        None
                    }
                    ready = bus.subscribe_when_registered::<WorkspaceEvent>(&workspaces_kind), if workspaces.is_none() => {
                        workspaces = Some(ready.map_err(|error| error.to_string())?);
                        None
                    }
                    event = receive_catalog_event(&mut contexts) => {
                        match event {
                            Ok(event) => Some(event),
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => None,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => { contexts = None; continue; }
                        }
                    }
                    event = receive_catalog_event(&mut workspaces) => {
                        match event {
                            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => None,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => { workspaces = None; continue; }
                        }
                    }
                };
                let shared = shared.clone();
                // Serialize effects and await blocking work even during shutdown:
                // cancellation cannot make an in-flight service call disappear.
                worker.spawn_blocking(move || {
                    if let Some(event) = event { handle_context_event(&shared, &event); }
                    else { publish_tab_list_snapshot(shared.caller.as_ref(), &shared.runtime_state); }
                }).await.map_err(|error| error.to_string())?;
            }
        }).map_err(PluginCommandError::failed)?;
        Ok(result)
    }

    fn register_typed_services(
        &self,
        context: TypedServiceRegistrationContext<'_>,
        registry: &mut TypedServiceRegistry,
    ) {
        let total_started = Instant::now();
        // Provider handles share the same `LastSelectedByClient` map
        // as the byte-encoded path on `TabsPlugin` so state stays
        // consistent between transports.
        let shared = TabsSharedState {
            caller: Arc::new(TypedServiceCaller::from_registration_context(&context)),
            last_selected_by_client: self.last_selected_by_client.clone(),
            runtime_state: self.runtime_state.clone(),
        };

        let handles_started = Instant::now();
        let commands: Arc<dyn TabsCommandsService + Send + Sync> =
            Arc::new(TabsCommandsHandle::new(shared.clone()));
        let _ = tabs_commands::register_provider(registry, commands);

        let state: Arc<dyn TabsStateService + Send + Sync> =
            Arc::new(TabsStateHandle::new(shared.clone()));
        let _ = tabs_state::register_provider(registry, state);
        let handle_register_us = handles_started.elapsed().as_micros();

        // Publish the initial tab-list snapshot populated from the
        // plugin's persisted `tabs.order` storage projected through
        // the current context list. The `register_state_channel` call
        // in `activate` registered an empty placeholder because
        // `activate` has no host access; now that we have a
        // `TypedServiceCaller` we publish the authoritative state
        // synchronously so:
        //
        //   - The server's `spawn_plugin_bus_state_forwarder` (which
        //     runs after us in bootstrap) reads the populated value
        //     when it calls `subscribe_state` to capture `initial`.
        //   - Attach clients connecting afterward see the correct
        //     tab order on first frame — no flash of `1:terminal`
        //     even when the server starts with pre-existing contexts
        //     restored from a prior session.
        //
        // `tabs.order` is persisted under
        // `<data_dir>/plugin-storage/bmux.tabs/tabs.order.bin`
        // by the kernel storage service, so the user sees their tab
        // order exactly as they left it before the server shutdown.
        let snapshot_started = Instant::now();
        publish_tab_list_snapshot(shared.caller.as_ref(), &shared.runtime_state);
        let snapshot_publish_us = snapshot_started.elapsed().as_micros();
        emit_tabs_plugin_phase_timing(
            &PhasePayload::new("bmux.tabs.typed_services")
                .field("plugin_id", "bmux.tabs")
                .field("handle_register_us", handle_register_us)
                .field("snapshot_publish_us", snapshot_publish_us)
                .field("total_us", total_started.elapsed().as_micros())
                .finish(),
        );
    }
}

async fn receive_catalog_event<E: Send + Sync>(
    receiver: &mut Option<tokio::sync::broadcast::Receiver<Arc<E>>>,
) -> Result<Arc<E>, tokio::sync::broadcast::error::RecvError> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// Dispatch a single `ContextEvent` against the tabs-plugin's
/// persisted tab order + active marker. Create/select event bursts
/// are coalesced so the hot `new-tab` path does not publish the
/// same tab list three times.
fn handle_context_event(
    shared: &TabsSharedState,
    event: &bmux_contexts_plugin_api::contexts_events::ContextEvent,
) {
    use bmux_contexts_plugin_api::contexts_events::ContextEvent;

    let caller = shared.caller.as_ref();
    match event {
        ContextEvent::Created { context_id, name } => {
            cache_known_context(&shared.runtime_state, *context_id, name.clone());
            let workspace_id = list_contexts(caller)
                .ok()
                .and_then(|contexts| {
                    contexts
                        .into_iter()
                        .find(|context| context.id == *context_id)
                })
                .map_or_else(Uuid::nil, |context| context_workspace_id(&context));
            let _ = append_context_to_workspace_order(caller, workspace_id, *context_id);
        }
        ContextEvent::Closed { context_id } => {
            remove_known_context(&shared.runtime_state, *context_id);
            let _ = remove_context_from_all_workspace_orders(caller, *context_id);
            publish_tab_list_snapshot(caller, &shared.runtime_state);
        }
        ContextEvent::Selected { context_id } => {
            let _ = mark_context_active(caller, &shared.runtime_state, *context_id);
            publish_tab_list_snapshot(caller, &shared.runtime_state);
        }
        ContextEvent::Renamed { context_id, name } => {
            cache_known_context(&shared.runtime_state, *context_id, Some(name.clone()));
            publish_tab_list_snapshot(caller, &shared.runtime_state);
        }
        ContextEvent::SessionActiveContextChanged { context_id, .. } => {
            if in_memory_runtime_context_id(&shared.runtime_state, ACTIVE_TAB_CONTEXT_KEY)
                == Some(*context_id)
            {
                return;
            }
            let _ = mark_context_active(caller, &shared.runtime_state, *context_id);
            publish_tab_list_snapshot(caller, &shared.runtime_state);
        }
    }
}

fn append_context_to_workspace_order(
    caller: &impl HostRuntimeApi,
    workspace_id: Uuid,
    context_id: Uuid,
) -> Result<(), String> {
    let mut order = get_stored_tab_order_ids_for_workspace(caller, workspace_id)?;
    if !order.contains(&context_id) {
        order.push(context_id);
        set_stored_tab_order_ids_for_workspace(caller, workspace_id, &order)?;
    }
    Ok(())
}

fn remove_context_from_all_workspace_orders(
    caller: &(impl HostRuntimeApi + Sync),
    context_id: Uuid,
) -> Result<(), String> {
    let mut client = dispatch_client(caller);
    let workspaces = bmux_plugin::block_on_typed_dispatch(
        bmux_workspaces_plugin_api::workspaces_state::client::list_workspaces(&mut client),
    )
    .unwrap_or_default();
    for workspace in workspaces {
        let mut order = get_stored_tab_order_ids_for_workspace(caller, workspace.id)?;
        let original_len = order.len();
        order.retain(|id| *id != context_id);
        if order.len() != original_len {
            set_stored_tab_order_ids_for_workspace(caller, workspace.id, &order)?;
        }
    }
    Ok(())
}

/// Append `context_id` to the persisted `tabs.order` list when it
/// is not already present. Preserves the existing order of every
/// already-known entry — new contexts land at the end, matching the
/// creation order of the `ContextEvent::Created` stream.
#[cfg(test)]
fn append_context_to_tab_order(
    caller: &impl HostRuntimeApi,
    runtime_state: &TabRuntimeStateHandle,
    context_id: Uuid,
) -> Result<(), String> {
    append_contexts_to_tab_order(caller, runtime_state, [context_id])
}

#[cfg(test)]
fn append_contexts_to_tab_order(
    caller: &impl HostRuntimeApi,
    runtime_state: &TabRuntimeStateHandle,
    context_ids: impl IntoIterator<Item = Uuid>,
) -> Result<(), String> {
    let Ok(mut state) = runtime_state.lock() else {
        return append_contexts_to_stored_tab_order(caller, context_ids);
    };
    let mut order_ids = if let Some(order_ids) = state.tab_order_ids.clone() {
        order_ids
    } else {
        let order_ids = get_stored_tab_order_ids(caller)?;
        state.tab_order_ids = Some(order_ids.clone());
        order_ids
    };
    let mut known_ids = order_ids.iter().copied().collect::<HashSet<_>>();
    let mut appended_ids = Vec::new();
    for context_id in context_ids {
        if known_ids.insert(context_id) {
            order_ids.push(context_id);
            appended_ids.push(context_id);
        }
    }
    if !appended_ids.is_empty() || state.tab_order_dirty {
        state.tab_order_ids = Some(order_ids.clone());
        set_stored_tab_order_ids(caller, &order_ids)?;
        state.tab_order_dirty = false;
    }
    Ok(())
}

fn cache_contexts_to_tab_order(
    runtime_state: &TabRuntimeStateHandle,
    context_ids: impl IntoIterator<Item = Uuid>,
) {
    let Ok(mut state) = runtime_state.lock() else {
        return;
    };
    let mut order_ids = state.tab_order_ids.clone().unwrap_or_default();
    let mut known_ids = order_ids.iter().copied().collect::<HashSet<_>>();
    let mut changed = false;
    for context_id in context_ids {
        if known_ids.insert(context_id) {
            order_ids.push(context_id);
            changed = true;
        }
    }
    if changed {
        state.tab_order_ids = Some(order_ids);
        state.tab_order_dirty = true;
    }
}

#[cfg(test)]
fn append_contexts_to_stored_tab_order(
    caller: &impl HostRuntimeApi,
    context_ids: impl IntoIterator<Item = Uuid>,
) -> Result<(), String> {
    let mut order_ids = get_stored_tab_order_ids(caller)?;
    let mut known_ids = order_ids.iter().copied().collect::<HashSet<_>>();
    let mut changed = false;
    for context_id in context_ids {
        if known_ids.insert(context_id) {
            order_ids.push(context_id);
            changed = true;
        }
    }
    if changed {
        set_stored_tab_order_ids(caller, &order_ids)?;
    }
    Ok(())
}

/// Remove `context_id` from the persisted `tabs.order` list.
/// No-op when the id is not present. Also clears the active marker
/// if it was pointing at the removed context.
#[cfg(test)]
fn remove_context_from_tab_order(
    caller: &impl HostRuntimeApi,
    runtime_state: &TabRuntimeStateHandle,
    context_id: Uuid,
) -> Result<(), String> {
    let mut order_ids = get_stored_tab_order_ids(caller)?;
    let len_before = order_ids.len();
    order_ids.retain(|id| *id != context_id);
    if order_ids.len() == len_before {
        // Not in list — nothing to persist. Still clear active if it
        // matches, below.
    } else {
        set_stored_tab_order_ids(caller, &order_ids)?;
    }
    if let Ok(mut state) = runtime_state.lock() {
        state.tab_order_ids = Some(order_ids);
        state.tab_order_dirty = false;
    }
    // Clear active marker if it points at the removed context.
    if let Ok(Some(active)) = get_runtime_context_id(caller, runtime_state, ACTIVE_TAB_CONTEXT_KEY)
        && active == context_id
    {
        let _ = clear_runtime_context_id(caller, runtime_state, ACTIVE_TAB_CONTEXT_KEY);
        let _ = set_stored_context_id(caller, ACTIVE_TAB_CONTEXT_KEY, None);
    }
    if let Ok(Some(previous)) =
        get_runtime_context_id(caller, runtime_state, PREVIOUS_TAB_CONTEXT_KEY)
        && previous == context_id
    {
        let _ = clear_runtime_context_id(caller, runtime_state, PREVIOUS_TAB_CONTEXT_KEY);
    }
    Ok(())
}

/// Update `ACTIVE_TAB_CONTEXT_KEY` to `context_id`, moving the
/// previous active context (if any and different) into
/// `PREVIOUS_TAB_CONTEXT_KEY` so `last-tab` still works.
fn mark_context_active_cached(
    runtime_state: &TabRuntimeStateHandle,
    previous_context: Option<Uuid>,
    context_id: Uuid,
) {
    if let Ok(mut state) = runtime_state.lock() {
        if let Some(previous) = previous_context
            && previous != context_id
        {
            state.previous_context_id = Some(previous);
        }
        state.active_context_id = Some(context_id);
    }
}

fn mark_context_active(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    context_id: Uuid,
) -> Result<(), String> {
    let previous = get_runtime_context_id(caller, runtime_state, ACTIVE_TAB_CONTEXT_KEY)
        .ok()
        .flatten();
    if previous == Some(context_id) {
        return Ok(());
    }
    if let Some(previous) = previous {
        let _ = set_runtime_context_id(
            caller,
            runtime_state,
            PREVIOUS_TAB_CONTEXT_KEY,
            Some(previous),
        );
    }
    set_runtime_context_id(
        caller,
        runtime_state,
        ACTIVE_TAB_CONTEXT_KEY,
        Some(context_id),
    )?;
    set_stored_context_id(caller, ACTIVE_TAB_CONTEXT_KEY, Some(context_id))
}

#[allow(clippy::too_many_lines)]
fn handle_command(plugin: &TabsPlugin, context: &NativeCommandContext) -> Result<(), String> {
    // Only emit confirmation text to stdout when invoked from a
    // standalone CLI (e.g. `bmux tab new`). When this plugin is
    // dispatched from an attach keybinding the host is rendering a
    // raw-mode TUI and `println!` would paint over pane content; the
    // attach runtime observes state changes (current context id,
    // context list) directly and refreshes from those, so silence is
    // correct there.
    let emit_to_stdout = matches!(
        context.invocation_source,
        bmux_plugin_sdk::NativeCommandInvocationSource::Cli
    );
    match context.command.as_str() {
        "new-tab" => {
            let name = option_value(&context.arguments, "name");
            let ack = create_tab(context, &plugin.runtime_state, name)?;
            record_selected_context_outcome(&ack);
            if emit_to_stdout && let Some(context_id) = ack.id {
                println!("created tab context: {context_id}");
            }
            Ok(())
        }
        "rename-tab" => {
            if let Some(name) = option_value(&context.arguments, "name") {
                let ack = rename_tab(context, &plugin.runtime_state, &name)?;
                if emit_to_stdout && let Some(context_id) = ack.id {
                    println!("renamed tab context: {context_id}");
                }
                return Ok(());
            }
            if !matches!(
                context.invocation_source,
                bmux_plugin_sdk::NativeCommandInvocationSource::AttachKeybinding
            ) {
                return Err("rename-tab requires --name when not invoked from attach".to_string());
            }
            spawn_rename_tab_prompt(context.clone(), Arc::clone(&plugin.runtime_state))?;
            Ok(())
        }
        "rename-tab-by-id" => {
            let id = option_value(&context.arguments, "id")
                .ok_or_else(|| "rename-tab-by-id requires --id".to_string())?;
            let id = Uuid::parse_str(&id).map_err(|error| format!("invalid --id: {error}"))?;
            let name = option_value(&context.arguments, "name")
                .ok_or_else(|| "rename-tab-by-id requires --name".to_string())?;
            let ack = rename_tab_by_id(context, &plugin.runtime_state, id, &name)?;
            if emit_to_stdout && let Some(context_id) = ack.id {
                println!("renamed tab context: {context_id}");
            }
            Ok(())
        }
        "list-tabs" => {
            let session_filter = option_value(&context.arguments, "session");
            let as_json = has_flag(&context.arguments, "json");
            let tabs = list_tabs(context, &plugin.runtime_state, session_filter.as_deref())?;
            if !emit_to_stdout {
                // Rendering list output is only meaningful from the
                // CLI; attach keybindings don't have a useful surface
                // for it here and the attach UI refreshes its own
                // state from the contexts/sessions catalogs.
                return Ok(());
            }
            if as_json {
                let output = serde_json::to_string_pretty(&serde_json::json!({ "tabs": tabs }))
                    .map_err(|error| error.to_string())?;
                println!("{output}");
            } else if tabs.is_empty() {
                println!("no tabs");
            } else {
                for tab in tabs {
                    println!(
                        "{}\t{}\t{}",
                        tab.id,
                        tab.name,
                        if tab.active { "active" } else { "inactive" }
                    );
                }
            }
            Ok(())
        }
        "kill-tab" => {
            let target = positional_value(&context.arguments)
                .ok_or_else(|| "missing required TARGET argument".to_string())?;
            let selector = parse_selector(&target)?;
            let force_local = has_flag(&context.arguments, "force-local");
            let closed_id = close_context(context, selector, force_local)?;
            if emit_to_stdout {
                println!("killed tab context: {closed_id}");
            }
            Ok(())
        }
        "kill-all-tabs" => {
            let force_local = has_flag(&context.arguments, "force-local");
            let contexts = list_contexts(context)?;
            if contexts.is_empty() {
                if emit_to_stdout {
                    println!("no tabs");
                }
                return Ok(());
            }
            for context_summary in contexts {
                let closed_id = close_context(
                    context,
                    context_selector_by_id(context_summary.id),
                    force_local,
                )?;
                if emit_to_stdout {
                    println!("killed tab context: {closed_id}");
                }
            }
            Ok(())
        }
        "switch-tab" => {
            let target = positional_value(&context.arguments)
                .ok_or_else(|| "missing required TARGET argument".to_string())?;
            let selector = parse_selector(&target)?;
            let ack = switch_tab(
                context,
                &plugin.runtime_state,
                selector,
                &plugin.last_selected_by_client,
                context.caller_client_id,
            )?;
            let context_id = ack
                .id
                .ok_or_else(|| "switch-tab did not return selected context id".to_string())?;
            bmux_plugin_sdk::record_command_outcome_metadata(
                COMMAND_OUTCOME_SELECTED_CONTEXT_ID_KEY,
                serde_json::json!(context_id),
            );
            if emit_to_stdout {
                println!("active tab context: {context_id}");
            }
            Ok(())
        }
        "move-tab" => {
            let source = positional_value_at(&context.arguments, 0)
                .ok_or_else(|| "missing required SOURCE_CONTEXT_ID argument".to_string())?;
            let target = positional_value_at(&context.arguments, 1)
                .ok_or_else(|| "missing required TARGET_CONTEXT_ID argument".to_string())?;
            let placement = option_value(&context.arguments, "placement")
                .ok_or_else(|| "--placement is required".to_string())?;
            let source_id = Uuid::parse_str(&source)
                .map_err(|error| format!("invalid source context id '{source}': {error}"))?;
            let target_id = Uuid::parse_str(&target)
                .map_err(|error| format!("invalid target context id '{target}': {error}"))?;
            let placement = parse_tab_move_placement_arg(&placement)?;
            let ack = move_tab(
                context,
                &plugin.runtime_state,
                source_id,
                target_id,
                placement,
            )?;
            if emit_to_stdout && let Some(id) = ack.id {
                println!("moved tab context: {id}");
            }
            Ok(())
        }
        "next-tab" => {
            let ack = cycle_tab(
                context,
                &plugin.runtime_state,
                TabCycleDirection::Next,
                &plugin.last_selected_by_client,
                context.caller_client_id,
            )?;
            record_selected_context_outcome(&ack);
            if emit_to_stdout && let Some(id) = ack.id {
                println!("next-tab selected context {id}");
            }
            Ok(())
        }
        "prev-tab" => {
            let ack = cycle_tab(
                context,
                &plugin.runtime_state,
                TabCycleDirection::Previous,
                &plugin.last_selected_by_client,
                context.caller_client_id,
            )?;
            record_selected_context_outcome(&ack);
            if emit_to_stdout && let Some(id) = ack.id {
                println!("prev-tab selected context {id}");
            }
            Ok(())
        }
        "last-tab" => {
            let ack = cycle_tab(
                context,
                &plugin.runtime_state,
                TabCycleDirection::Last,
                &plugin.last_selected_by_client,
                context.caller_client_id,
            )?;
            record_selected_context_outcome(&ack);
            if emit_to_stdout && let Some(id) = ack.id {
                println!("last-tab selected context {id}");
            }
            Ok(())
        }
        "goto-tab" => {
            let index_str = positional_value(&context.arguments)
                .ok_or_else(|| "missing required INDEX argument".to_string())?;
            let index: usize = index_str.parse().map_err(|_| {
                format!("invalid tab index '{index_str}' (expected 1-based number)")
            })?;
            if index == 0 {
                return Err("tab index must be 1 or greater".to_string());
            }
            let ack = goto_tab_by_index(
                context,
                &plugin.runtime_state,
                index,
                &plugin.last_selected_by_client,
                context.caller_client_id,
            )?;
            record_selected_context_outcome(&ack);
            if emit_to_stdout && let Some(id) = ack.id {
                println!("goto-tab {index} selected context {id}");
            }
            Ok(())
        }
        "close-current-tab" => {
            let ack = close_current_tab(
                context,
                &plugin.runtime_state,
                &plugin.last_selected_by_client,
                context.caller_client_id,
                context.settings.as_ref(),
            )?;
            if emit_to_stdout && let Some(id) = ack.id {
                println!("closed current tab context {id}");
            }
            Ok(())
        }
        "reset-order" => {
            let count = reset_tab_order(context, &plugin.runtime_state)?;
            if emit_to_stdout {
                println!("reset tab order; rebuilt {count} tabs");
            }
            Ok(())
        }
        // ── Pane-level commands (promoted from service handlers) ──
        //
        // Each of these dispatches to the same typed-service logic
        // implemented in `invoke_service`, but via a command-style
        // entry so keybindings can reach them through
        // `plugin:bmux.tabs:<name>`. The handlers forward to the
        // `HostRuntimeApi::pane_*` trait methods which ultimately
        // route through the tabs-plugin service boundary.
        //
        // Keybindings do not pass a `--session` arg (the attach
        // runtime always operates on the currently-attached session),
        // so we pass `session: None` to the underlying request and
        // rely on the host to resolve to the caller's attached
        // session.
        "focus-pane-in-direction" => {
            let direction = option_value(&context.arguments, "direction")
                .ok_or_else(|| "--direction is required".to_string())?;
            let direction = parse_pane_direction_arg(&direction)?;
            let focus_dir = focus_direction_name(direction).ok_or_else(|| {
                "direction must be left/right/up/down/next/prev (horizontal/vertical are split-only)".to_string()
            })?;
            focus_pane(context, None, None, focus_dir)?;
            Ok(())
        }
        "split-pane" => {
            let direction = option_value(&context.arguments, "direction")
                .ok_or_else(|| "--direction is required".to_string())?;
            let direction = parse_pane_direction_arg(&direction)?;
            split_pane(context, None, None, direction, None)?;
            Ok(())
        }
        "resize-pane" => {
            let direction_arg = option_value(&context.arguments, "direction");
            let direction = direction_arg.as_deref().map_or(
                Ok(PaneResizeDirection::Increase),
                parse_pane_resize_direction_arg,
            )?;
            resize_pane(context, None, None, direction, 1)?;
            Ok(())
        }
        "zoom-pane" => {
            zoom_pane(context, None)?;
            Ok(())
        }
        "create-floating-pane" => {
            let mut options = floating_pane_defaults(context.settings.as_ref())?;
            options.overlay_cli_arguments(&context.arguments)?;
            let ack = create_floating_pane_command(context, options)?;
            if emit_to_stdout {
                println!("created floating pane: {}", ack.pane_id);
            }
            Ok(())
        }
        "list-floating-panes" => {
            let session_id = option_value(&context.arguments, "session")
                .map(|value| {
                    Uuid::parse_str(&value).map_err(|_| format!("invalid session id '{value}'"))
                })
                .transpose()?;
            let panes = list_floating_panes(context, session_id)?;
            if emit_to_stdout {
                for (session_id, pane) in panes {
                    println!(
                        "{} session={} scope={} layer={} z={} rect={}x{}+{}+{} visible={}",
                        pane.pane_id,
                        session_id,
                        pane.scope,
                        pane.layer,
                        pane.z,
                        pane.w,
                        pane.h,
                        pane.x,
                        pane.y,
                        pane.visible
                    );
                }
            }
            Ok(())
        }
        "focus-next-floating-pane" => {
            focus_next_floating_pane(context, None)?;
            Ok(())
        }
        "raise-active-floating-pane" => {
            mutate_active_floating_pane(context, None, "raise")?;
            Ok(())
        }
        "lower-active-floating-pane" => {
            mutate_active_floating_pane(context, None, "lower")?;
            Ok(())
        }
        "move-active-floating-pane" => {
            let direction = option_value(&context.arguments, "direction")
                .ok_or_else(|| "--direction is required".to_string())?;
            let direction = parse_floating_move_direction_arg(&direction)?;
            let cells = option_value(&context.arguments, "cells")
                .map(|value| parse_u16_arg(&value, "cells"))
                .transpose()?
                .unwrap_or(1);
            move_active_floating_pane(context, None, direction, cells)?;
            Ok(())
        }
        "resize-active-floating-pane" => {
            let direction = option_value(&context.arguments, "direction");
            let direction = direction.as_deref().map_or(
                Ok(PaneResizeDirection::Increase),
                parse_pane_resize_direction_arg,
            )?;
            let cells = option_value(&context.arguments, "cells")
                .map(|value| parse_u16_arg(&value, "cells"))
                .transpose()?
                .unwrap_or(1);
            resize_active_floating_pane(context, None, direction, cells)?;
            Ok(())
        }
        "close-active-floating-pane" => {
            mutate_active_floating_pane(context, None, "close")?;
            Ok(())
        }
        "focus-floating-pane"
        | "raise-floating-pane"
        | "lower-floating-pane"
        | "close-floating-pane" => {
            let pane_id = parse_pane_id_argument(&context.arguments)?;
            let command = context.command.trim_end_matches("-floating-pane");
            mutate_floating_pane_command(context, command, pane_id)?;
            Ok(())
        }
        "move-floating-pane" => {
            let pane_id = parse_pane_id_argument(&context.arguments)?;
            let origin_x = option_value(&context.arguments, "x")
                .ok_or_else(|| "--x is required".to_string())
                .and_then(|value| parse_u16_arg(&value, "x"))?;
            let origin_y = option_value(&context.arguments, "y")
                .ok_or_else(|| "--y is required".to_string())
                .and_then(|value| parse_u16_arg(&value, "y"))?;
            move_floating_pane_command(context, pane_id, origin_x, origin_y)?;
            Ok(())
        }
        "resize-floating-pane" => {
            let pane_id = parse_pane_id_argument(&context.arguments)?;
            let width = option_value(&context.arguments, "w")
                .ok_or_else(|| "--w is required".to_string())
                .and_then(|value| parse_u16_arg(&value, "w"))?;
            let height = option_value(&context.arguments, "h")
                .ok_or_else(|| "--h is required".to_string())
                .and_then(|value| parse_u16_arg(&value, "h"))?;
            resize_floating_pane_command(context, pane_id, width, height)?;
            Ok(())
        }
        "set-floating-pane-z" => {
            let pane_id = parse_pane_id_argument(&context.arguments)?;
            let z = option_value(&context.arguments, "z")
                .ok_or_else(|| "--z is required".to_string())
                .and_then(|value| parse_i32_arg(&value, "z"))?;
            set_floating_pane_z_command(context, pane_id, z)?;
            Ok(())
        }
        "set-floating-pane-layer" => {
            let pane_id = parse_pane_id_argument(&context.arguments)?;
            let layer = option_value(&context.arguments, "layer")
                .ok_or_else(|| "--layer is required".to_string())?;
            set_floating_pane_layer_command(context, pane_id, layer)?;
            Ok(())
        }
        "close-active-pane" => {
            close_pane(context, None, None)?;
            Ok(())
        }
        "restart-pane" => {
            // Mirrors `close-active-pane`: the active pane of the
            // caller's selected session is the target.
            restart_pane(context, None, None)?;
            Ok(())
        }
        _ => Err(format!("unsupported command '{}'", context.command)),
    }
}

fn record_selected_context_outcome(ack: &TabAck) {
    if let Some(context_id) = ack.id.as_deref() {
        bmux_plugin_sdk::record_command_outcome_metadata(
            COMMAND_OUTCOME_SELECTED_CONTEXT_ID_KEY,
            serde_json::json!(context_id),
        );
    }
}

#[derive(Debug)]
enum TabCycleDirection {
    Next,
    Previous,
    Last,
}

fn context_workspace_id(context: &ContextSummary) -> Uuid {
    let attribute = context
        .attributes
        .get("workspace")
        .map_or("default", String::as_str);
    if attribute == "default" {
        return Uuid::nil();
    }
    Uuid::parse_str(attribute).unwrap_or_else(|_| Uuid::nil())
}

fn workspace_names(caller: &(impl ServiceCaller + Sync)) -> BTreeMap<Uuid, String> {
    let mut client = dispatch_client(caller);
    bmux_plugin::block_on_typed_dispatch(
        bmux_workspaces_plugin_api::workspaces_state::client::list_workspaces(&mut client),
    )
    .unwrap_or_default()
    .into_iter()
    .map(|workspace| (workspace.id, workspace.name))
    .collect()
}

fn list_tabs(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    session_filter: Option<&str>,
) -> Result<Vec<TabEntry>, String> {
    let contexts = list_contexts(caller)?;
    let contexts = order_contexts_for_navigation(caller, runtime_state, contexts)?;
    let selected = if let Some(filter) = session_filter {
        let selector = parse_selector(filter)?;
        contexts
            .into_iter()
            .filter(|context| selector_matches_context(&selector, context))
            .collect::<Vec<_>>()
    } else {
        contexts
    };
    let current_context =
        resolve_effective_current_context_with_contexts(caller, runtime_state, &selected)?;

    let workspace_names = workspace_names(caller);
    Ok(selected
        .into_iter()
        .enumerate()
        .map(|(index, context)| {
            let workspace_id = context_workspace_id(&context);
            let workspace = workspace_names
                .get(&workspace_id)
                .cloned()
                .unwrap_or_else(|| "default".to_string());
            TabEntry {
                id: context.id.to_string(),
                name: context
                    .name
                    .unwrap_or_else(|| format!("tab-{}", index.saturating_add(1))),
                active: current_context == Some(context.id),
                workspace,
                workspace_id,
            }
        })
        .collect())
}

/// Monotonic counter for the tabs-list state channel.
///
/// Advanced once per [`publish_tab_list_snapshot`] call so
/// subscribers can deduplicate or order updates without relying on
/// wall-clock time.
static TAB_LIST_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Publish the current ordered tab list on the `tabs-list`
/// state channel.
///
/// Called by every tab-order-mutating code path (`create_tab`,
/// `switch_tab`, `kill_tab`, `kill_all_tabs`,
/// `goto_tab_by_index`, `cycle_tab`, `close_current_tab`) so
/// subscribers (the attach tab bar, future UI plugins) observe the
/// current order synchronously on `subscribe_state` and receive live
/// updates on every mutation — no polling.
///
/// Silently no-ops when the underlying `list_tabs` call fails or
/// when the state channel has not been registered (plugin not yet
/// activated). The channel is seeded empty in `activate`, so once the
/// plugin is active this publish always succeeds.
fn publish_tab_list_snapshot(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
) {
    let Ok(entries) = list_tabs(caller, runtime_state, None) else {
        return;
    };
    publish_tab_list_entries(entries);
}

fn publish_tab_list_ordered_contexts(
    caller: &(impl ServiceCaller + Sync),
    contexts: Vec<ContextSummary>,
    active_context_id: Option<Uuid>,
) {
    // A shared catalog must never become a single caller's workspace view.
    let mut contexts = contexts;
    if let Ok(all) = list_contexts(caller) {
        for context in all {
            if !contexts.iter().any(|known| known.id == context.id) {
                contexts.push(context);
            }
        }
    }
    let workspace_names = workspace_names(caller);
    let entries = contexts
        .into_iter()
        .enumerate()
        .map(|(index, context)| {
            let workspace_id = context_workspace_id(&context);
            let workspace = workspace_names
                .get(&workspace_id)
                .cloned()
                .unwrap_or_else(|| "default".to_string());
            TabEntry {
                id: context.id.to_string(),
                name: context
                    .name
                    .unwrap_or_else(|| format!("tab-{}", index.saturating_add(1))),
                active: active_context_id == Some(context.id),
                workspace,
                workspace_id,
            }
        })
        .collect();
    publish_tab_list_entries(entries);
}

fn publish_tab_list_entries(entries: Vec<TabEntry>) {
    let tabs: Vec<bmux_tabs_plugin_api::tabs_list::TabListEntry> = entries
        .into_iter()
        .filter_map(|entry| {
            let id = Uuid::parse_str(&entry.id).ok()?;
            Some(bmux_tabs_plugin_api::tabs_list::TabListEntry {
                id,
                name: entry.name,
                active: entry.active,
                workspace: entry.workspace,
                workspace_id: entry.workspace_id,
            })
        })
        .collect();
    let revision = TAB_LIST_REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let snapshot = bmux_tabs_plugin_api::tabs_list::TabListSnapshot { tabs, revision };
    let _ = bmux_plugin::global_event_bus()
        .publish_state(&bmux_tabs_plugin_api::tabs_list::STATE_KIND, snapshot);
}

/// Clear persisted `tabs.order` and rebuild deterministically from
/// the current context list.
///
/// Serves as an escape hatch for users whose tabs.order got
/// scrambled by pre-event-driven code paths (legacy bug). Ordering is
/// reconstructed from the context list sorted by UUID, so every
/// invocation produces the same result given the same input — but it
/// is NOT guaranteed to match creation order. Users who want exact
/// creation order should recreate their contexts after reset.
///
/// Returns the count of contexts written to the new order.
fn reset_tab_order(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
) -> Result<usize, String> {
    let contexts = list_contexts_in_active_workspace(caller)?;
    let mut ids: Vec<Uuid> = contexts.iter().map(|context| context.id).collect();
    ids.sort_by_key(uuid::Uuid::as_u128);
    set_stored_tab_order_ids_for_workspace(caller, active_workspace_id(caller), &ids)?;
    if let Ok(mut state) = runtime_state.lock() {
        state.tab_order_ids = Some(ids.clone());
        state.tab_order_dirty = false;
    }
    publish_tab_list_snapshot(caller, runtime_state);
    Ok(ids.len())
}

fn active_workspace_attribute(caller: &(impl ServiceCaller + Sync)) -> String {
    let mut client = dispatch_client(caller);
    let workspace = bmux_plugin::block_on_typed_dispatch(
        bmux_workspaces_plugin_api::workspaces_state::client::current_workspace(&mut client),
    )
    .ok()
    .flatten();
    workspace.map_or_else(
        || "default".to_string(),
        |workspace| {
            if workspace.id.is_nil() {
                "default".to_string()
            } else {
                workspace.id.to_string()
            }
        },
    )
}

fn create_tab(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    name: Option<String>,
) -> Result<TabAck, String> {
    let mut contexts = list_contexts_in_active_workspace(caller)?;
    seed_known_contexts(runtime_state, &contexts);
    let resolved_name = name.or_else(|| Some(next_default_tab_name_for_contexts(&contexts)));
    let previous_context =
        resolve_effective_current_context_with_contexts(caller, runtime_state, &contexts)
            .ok()
            .flatten();
    let mut attributes = BTreeMap::new();
    attributes.insert("workspace".to_string(), active_workspace_attribute(caller));
    let context = create_context(caller, resolved_name, attributes)?;
    let context_id = context.id;
    cache_known_context(runtime_state, context_id, context.name.clone());
    contexts.push(context);
    let mut order_appends = Vec::with_capacity(2);
    if let Some(previous) = previous_context {
        order_appends.push(previous);
    }
    order_appends.push(context_id);
    cache_contexts_to_tab_order(runtime_state, order_appends);
    mark_context_active_cached(runtime_state, previous_context, context_id);
    publish_tab_list_ordered_contexts(caller, contexts, Some(context_id));
    Ok(TabAck {
        ok: true,
        id: Some(context_id.to_string()),
    })
}

fn normalize_tab_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("name must not be empty".to_string());
    }
    Ok(trimmed.to_string())
}

fn rename_tab(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    name: &str,
) -> Result<TabAck, String> {
    let name = normalize_tab_name(name)?;
    let contexts = list_contexts_in_active_workspace(caller)?;
    let contexts = order_contexts_for_navigation(caller, runtime_state, contexts)?;
    let context_id =
        resolve_effective_current_context_with_contexts(caller, runtime_state, &contexts)?
            .ok_or_else(|| "no current tab to rename".to_string())?;
    rename_tab_context(caller, runtime_state, context_id, name)
}

/// Rename a specific tab by id, regardless of which tab is current.
fn rename_tab_by_id(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    id: Uuid,
    name: &str,
) -> Result<TabAck, String> {
    let name = normalize_tab_name(name)?;
    let contexts = list_contexts(caller)?;
    if !contexts.iter().any(|context| context.id == id) {
        return Err(format!("unknown tab {id}"));
    }
    rename_tab_context(caller, runtime_state, id, name)
}

/// Shared rename tail: apply the context rename, refresh the local cache, and
/// republish the tab list so subscribers (the attach tab bar) see the change.
fn rename_tab_context(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    context_id: Uuid,
    name: String,
) -> Result<TabAck, String> {
    let renamed_id = rename_context(caller, context_selector_by_id(context_id), name.clone())?;
    cache_known_context(runtime_state, renamed_id, Some(name));
    publish_tab_list_snapshot(caller, runtime_state);
    Ok(TabAck {
        ok: true,
        id: Some(renamed_id.to_string()),
    })
}

fn current_tab_label(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
) -> Option<String> {
    list_tabs(caller, runtime_state, None)
        .ok()?
        .into_iter()
        .find(|tab| tab.active)
        .map(|tab| tab.name)
}

fn spawn_rename_tab_prompt(
    context: NativeCommandContext,
    runtime_state: TabRuntimeStateHandle,
) -> Result<(), String> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| "rename-tab prompt requires attach runtime".to_string())?;
    handle.spawn(async move {
        prompt_and_rename_tab(context, runtime_state).await;
    });
    Ok(())
}

async fn prompt_and_rename_tab(
    context: NativeCommandContext,
    runtime_state: TabRuntimeStateHandle,
) {
    let initial = current_tab_label(&context, &runtime_state).unwrap_or_default();
    let request = PromptRequest::text_input("Rename tab")
        .message("Enter a new name for the current tab.")
        .submit_label("Rename")
        .owner_plugin_id("bmux.tabs")
        .modal_id("rename-tab")
        .policy(PromptPolicy::Enqueue)
        .input_initial(initial)
        .input_required(true)
        .input_validation(PromptValidation::NonEmpty);
    let response = match prompt::request(request).await {
        Ok(response) => response,
        Err(error) => {
            log_tab_rename_error(&context, format!("failed opening rename prompt: {error}"));
            return;
        }
    };
    let PromptResponse::Submitted(PromptValue::Text(name)) = response else {
        return;
    };
    if let Err(error) = rename_tab(&context, &runtime_state, &name) {
        log_tab_rename_error(&context, format!("rename-tab failed: {error}"));
    }
}

fn log_tab_rename_error(context: &impl HostRuntimeApi, message: String) {
    let _ = context.log_write(&LogWriteRequest {
        level: LogWriteLevel::Warn,
        message,
        target: Some("bmux.tabs".to_string()),
    });
}

fn next_default_tab_name_for_contexts(contexts: &[ContextSummary]) -> String {
    let mut next = 1_u32;
    loop {
        let candidate = format!("tab-{next}");
        if contexts
            .iter()
            .all(|context| context.name.as_deref() != Some(candidate.as_str()))
        {
            return candidate;
        }
        next = next.saturating_add(1);
    }
}

fn kill_tab(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    selector: ContextSelector,
    force_local: bool,
) -> Result<TabAck, String> {
    let context_id = close_context(caller, selector, force_local)?;
    publish_tab_list_snapshot(caller, runtime_state);
    Ok(TabAck {
        ok: true,
        id: Some(context_id.to_string()),
    })
}

fn kill_all_tabs(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    force_local: bool,
) -> Result<TabAck, String> {
    let contexts = list_contexts(caller)?;
    for context in contexts {
        close_context(caller, context_selector_by_id(context.id), force_local)?;
    }
    publish_tab_list_snapshot(caller, runtime_state);
    Ok(TabAck { ok: true, id: None })
}

#[allow(clippy::needless_pass_by_value)] // Plugin command dispatch passes owned selector from deserialized request
fn switch_tab(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    selector: ContextSelector,
    last_selected_by_client: &LastSelectedByClient,
    caller_client_id: Option<Uuid>,
) -> Result<TabAck, String> {
    let total_started = Instant::now();
    let list_started = Instant::now();
    let contexts = list_contexts_in_active_workspace(caller)?;
    let context_list_us = list_started.elapsed().as_micros();
    let contexts = order_contexts_for_navigation(caller, runtime_state, contexts)?;
    switch_tab_with_contexts(
        caller,
        runtime_state,
        &selector,
        last_selected_by_client,
        caller_client_id,
        &contexts,
        SwitchTabTiming {
            context_list_us,
            total_started,
        },
    )
}

fn switch_tab_with_contexts(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    selector: &ContextSelector,
    last_selected_by_client: &LastSelectedByClient,
    caller_client_id: Option<Uuid>,
    contexts: &[ContextSummary],
    timing: SwitchTabTiming,
) -> Result<TabAck, String> {
    let resolve_started = Instant::now();
    let previous_context =
        resolve_effective_current_context_with_contexts(caller, runtime_state, contexts)?;
    let context_id = resolve_context_id_from_contexts(contexts, selector)?;
    let resolve_us = resolve_started.elapsed().as_micros();
    let select_started = Instant::now();
    select_context(caller, context_selector_by_id(context_id))?;
    let context_select_us = select_started.elapsed().as_micros();
    let remember_started = Instant::now();
    let remembered_for_client = if let Some(client_id) = caller_client_id
        && let Some(previous) = previous_context
        && previous != context_id
        && let Ok(mut map) = last_selected_by_client.lock()
    {
        map.insert(client_id, previous);
        true
    } else {
        false
    };
    if let Some(previous) = previous_context
        && previous != context_id
        && !remembered_for_client
    {
        let _ = set_runtime_context_id(
            caller,
            runtime_state,
            PREVIOUS_TAB_CONTEXT_KEY,
            Some(previous),
        );
    }
    let _ = set_runtime_context_id(
        caller,
        runtime_state,
        ACTIVE_TAB_CONTEXT_KEY,
        Some(context_id),
    );
    let remember_us = remember_started.elapsed().as_micros();
    let publish_started = Instant::now();
    publish_tab_list_ordered_contexts(caller, contexts.to_vec(), Some(context_id));
    let publish_us = publish_started.elapsed().as_micros();
    emit_attach_phase_timing(&serde_json::json!({
        "phase": "tabs.switch_tab",
        "previous_context_id": previous_context,
        "selected_context_id": context_id,
        "context_count": contexts.len(),
        "context_list_us": timing.context_list_us,
        "resolve_us": resolve_us,
        "context_select_us": context_select_us,
        "remember_us": remember_us,
        "publish_us": publish_us,
        "total_us": timing.total_started.elapsed().as_micros(),
    }));
    Ok(TabAck {
        ok: true,
        id: Some(context_id.to_string()),
    })
}

fn move_tab(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    source: Uuid,
    target: Uuid,
    placement: TabMovePlacement,
) -> Result<TabAck, String> {
    if source == target {
        return Ok(TabAck {
            ok: true,
            id: Some(source.to_string()),
        });
    }

    let contexts = list_contexts_in_active_workspace(caller)?;
    let live_ids = contexts
        .iter()
        .map(|context| context.id)
        .collect::<HashSet<_>>();
    if !live_ids.contains(&source) {
        return Err(format!("source tab context not found: {source}"));
    }
    if !live_ids.contains(&target) {
        return Err(format!("target tab context not found: {target}"));
    }

    let mut order_ids = resolve_tab_order_ids(caller, runtime_state, &contexts)?;
    let Some(source_index) = order_ids.iter().position(|id| *id == source) else {
        return Err(format!("source tab context not in order: {source}"));
    };
    let source_id = order_ids.remove(source_index);
    let Some(target_index) = order_ids.iter().position(|id| *id == target) else {
        return Err(format!("target tab context not in order: {target}"));
    };
    let insert_index = match placement {
        TabMovePlacement::Before => target_index,
        TabMovePlacement::After => target_index.saturating_add(1),
    };
    order_ids.insert(insert_index.min(order_ids.len()), source_id);

    set_stored_tab_order_ids_for_workspace(caller, active_workspace_id(caller), &order_ids)?;
    if let Ok(mut state) = runtime_state.lock() {
        state.tab_order_ids = Some(order_ids);
        state.tab_order_dirty = false;
    }
    publish_tab_list_snapshot(caller, runtime_state);
    Ok(TabAck {
        ok: true,
        id: Some(source.to_string()),
    })
}

#[derive(Clone, Copy)]
struct SwitchTabTiming {
    context_list_us: u128,
    total_started: Instant,
}

#[allow(clippy::needless_pass_by_value)] // Plugin command dispatch passes owned direction from deserialized request
fn cycle_tab(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    direction: TabCycleDirection,
    last_selected_by_client: &LastSelectedByClient,
    caller_client_id: Option<Uuid>,
) -> Result<TabAck, String> {
    let total_started = Instant::now();
    let list_started = Instant::now();
    let contexts = list_contexts_in_active_workspace(caller)?;
    let context_list_us = list_started.elapsed().as_micros();
    let order_started = Instant::now();
    let contexts = order_contexts_for_navigation(caller, runtime_state, contexts)?;
    let order_us = order_started.elapsed().as_micros();
    if contexts.len() < 2 {
        return Err("no alternate tab available".to_string());
    }
    let resolve_started = Instant::now();
    let current_context =
        resolve_effective_current_context_with_contexts(caller, runtime_state, &contexts)?
            .unwrap_or(contexts[0].id);
    let current_index = contexts
        .iter()
        .position(|context| context.id == current_context)
        .unwrap_or(0);
    let target_id = match direction {
        TabCycleDirection::Next => contexts[(current_index + 1) % contexts.len()].id,
        TabCycleDirection::Previous => {
            contexts[(current_index + contexts.len() - 1) % contexts.len()].id
        }
        TabCycleDirection::Last => {
            let remembered_by_client = caller_client_id.and_then(|client_id| {
                last_selected_by_client
                    .lock()
                    .ok()
                    .and_then(|map| map.get(&client_id).copied())
            });
            let remembered = remembered_by_client
                .or_else(|| {
                    get_runtime_context_id(caller, runtime_state, PREVIOUS_TAB_CONTEXT_KEY)
                        .ok()
                        .flatten()
                })
                .ok_or_else(|| "no previously active tab available".to_string())?;
            if !contexts.iter().any(|context| context.id == remembered) {
                return Err("no previously active tab available".to_string());
            }
            if remembered == current_context {
                return Err("no previously active tab available".to_string());
            }
            remembered
        }
    };
    let resolve_us = resolve_started.elapsed().as_micros();
    emit_attach_phase_timing(&serde_json::json!({
        "phase": "tabs.cycle_tab",
        "direction": format!("{direction:?}"),
        "current_context_id": current_context,
        "target_context_id": target_id,
        "context_count": contexts.len(),
        "context_list_us": context_list_us,
        "order_us": order_us,
        "resolve_us": resolve_us,
        "pre_switch_us": total_started.elapsed().as_micros(),
    }));
    switch_tab_with_contexts(
        caller,
        runtime_state,
        &context_selector_by_id(target_id),
        last_selected_by_client,
        caller_client_id,
        &contexts,
        SwitchTabTiming {
            context_list_us,
            total_started,
        },
    )
}

fn goto_tab_by_index(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    index: usize,
    last_selected_by_client: &LastSelectedByClient,
    caller_client_id: Option<Uuid>,
) -> Result<TabAck, String> {
    let total_started = Instant::now();
    if index == 0 {
        return Err("tab index must be 1 or greater".to_string());
    }
    let list_started = Instant::now();
    let contexts = list_contexts_in_active_workspace(caller)?;
    let context_list_us = list_started.elapsed().as_micros();
    let contexts = order_contexts_for_navigation(caller, runtime_state, contexts)?;
    if contexts.is_empty() {
        return Err("no tabs available".to_string());
    }
    let zero_based = index - 1;
    if zero_based >= contexts.len() {
        return Err(format!(
            "tab index {index} out of range (have {} tab{})",
            contexts.len(),
            if contexts.len() == 1 { "" } else { "s" }
        ));
    }
    let target_id = contexts[zero_based].id;
    switch_tab_with_contexts(
        caller,
        runtime_state,
        &context_selector_by_id(target_id),
        last_selected_by_client,
        caller_client_id,
        &contexts,
        SwitchTabTiming {
            context_list_us,
            total_started,
        },
    )
}

fn on_last_tab_closed_setting(settings: Option<&toml::Value>) -> Result<&'static str, String> {
    let Some(value) = settings.and_then(|settings| settings.get("on_last_tab_closed")) else {
        return Ok("delete");
    };
    match value.as_str() {
        Some("delete") => Ok("delete"),
        Some("keep_empty") => Ok("keep_empty"),
        Some(other) => Err(format!(
            "invalid on_last_tab_closed value '{other}' (expected delete or keep_empty)"
        )),
        None => Err("invalid on_last_tab_closed value (expected string)".to_string()),
    }
}

fn kill_active_workspace(caller: &(impl ServiceCaller + Sync)) -> Result<(), String> {
    let mut client = dispatch_client(caller);
    let current = bmux_plugin::block_on_typed_dispatch(
        bmux_workspaces_plugin_api::workspaces_state::client::current_workspace(&mut client),
    )
    .map_err(|error| typed_service_error("workspaces-state/current-workspace", error))?
    .ok_or_else(|| "no active workspace".to_string())?;
    let result = bmux_plugin::block_on_typed_dispatch(
        bmux_workspaces_plugin_api::workspaces_commands::client::kill_workspace(
            &mut client,
            bmux_workspaces_plugin_api::workspaces_state::WorkspaceSelector {
                id: Some(current.id),
                name: None,
            },
        ),
    )
    .map_err(|error| typed_service_error("workspaces-commands/kill-workspace", error))?;
    result
        .map(|_| ())
        .map_err(|error| format!("kill-workspace failed: {error:?}"))
}

fn close_current_tab(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    last_selected_by_client: &LastSelectedByClient,
    caller_client_id: Option<Uuid>,
    settings: Option<&toml::Value>,
) -> Result<TabAck, String> {
    let contexts = list_contexts_in_active_workspace(caller)?;
    let contexts = order_contexts_for_navigation(caller, runtime_state, contexts)?;
    let current_id =
        resolve_effective_current_context_with_contexts(caller, runtime_state, &contexts)?
            .ok_or_else(|| "no current tab to close".to_string())?;

    // If there is another tab to switch to, do so before closing.
    if contexts.len() > 1 {
        let current_index = contexts
            .iter()
            .position(|context| context.id == current_id)
            .unwrap_or(0);
        // Switch to the next tab (wrapping), or previous if we are at the end.
        let fallback_index = if current_index + 1 < contexts.len() {
            current_index + 1
        } else {
            current_index.saturating_sub(1)
        };
        let fallback_id = contexts[fallback_index].id;
        let _ = switch_tab(
            caller,
            runtime_state,
            context_selector_by_id(fallback_id),
            last_selected_by_client,
            caller_client_id,
        );
    }

    close_context(caller, context_selector_by_id(current_id), false)?;
    if contexts.len() == 1 && on_last_tab_closed_setting(settings)? == "delete" {
        kill_active_workspace(caller)?;
    }

    publish_tab_list_snapshot(caller, runtime_state);
    Ok(TabAck {
        ok: true,
        id: Some(current_id.to_string()),
    })
}

fn resolve_context_id_from_contexts(
    contexts: &[ContextSummary],
    selector: &ContextSelector,
) -> Result<Uuid, String> {
    contexts
        .iter()
        .find(|context| selector_matches_context(selector, context))
        .map(|context| context.id)
        .ok_or_else(|| "target context not found".to_string())
}

fn resolve_effective_current_context_with_contexts(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    contexts: &[ContextSummary],
) -> Result<Option<Uuid>, String> {
    let stored_active = in_memory_runtime_context_id(runtime_state, ACTIVE_TAB_CONTEXT_KEY)
        .filter(|id| contexts.iter().any(|context| context.id == *id));
    if stored_active.is_some() {
        return Ok(stored_active);
    }
    let current = current_context(caller)?
        .map(|context| context.id)
        .filter(|id| contexts.iter().any(|context| context.id == *id));
    if current.is_some() {
        return Ok(current);
    }
    let stored_active = get_runtime_context_id(caller, runtime_state, ACTIVE_TAB_CONTEXT_KEY)?
        .filter(|id| contexts.iter().any(|context| context.id == *id));
    Ok(stored_active)
}

fn get_stored_context_id(caller: &impl HostRuntimeApi, key: &str) -> Result<Option<Uuid>, String> {
    let response = caller
        .storage_get(&StorageGetRequest::new(storage_key(key)))
        .map_err(|error| error.to_string())?;
    let Some(value) = response.value else {
        return Ok(None);
    };
    let text = String::from_utf8(value).map_err(|error| error.to_string())?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    let id = Uuid::parse_str(text.trim()).map_err(|error| error.to_string())?;
    Ok(Some(id))
}

fn get_runtime_context_id(
    caller: &impl HostRuntimeApi,
    runtime_state: &TabRuntimeStateHandle,
    key: &str,
) -> Result<Option<Uuid>, String> {
    if let Some(context_id) = in_memory_runtime_context_id(runtime_state, key) {
        return Ok(Some(context_id));
    }
    if let Some(id) = get_volatile_context_id(caller, key)? {
        return Ok(Some(id));
    }
    get_stored_context_id(caller, key)
}

fn in_memory_runtime_context_id(runtime_state: &TabRuntimeStateHandle, key: &str) -> Option<Uuid> {
    let state = runtime_state.lock().ok()?;
    match key {
        ACTIVE_TAB_CONTEXT_KEY => state.active_context_id,
        PREVIOUS_TAB_CONTEXT_KEY => state.previous_context_id,
        _ => None,
    }
}

fn get_volatile_context_id(
    caller: &impl HostRuntimeApi,
    key: &str,
) -> Result<Option<Uuid>, String> {
    let response = caller
        .call_service::<_, bmux_plugin_sdk::VolatileStateGetResponse>(
            "bmux.storage",
            bmux_plugin_sdk::ServiceKind::Query,
            "volatile-state-query/v1",
            "get",
            &VolatileStateGetRequest::new(storage_key(key)),
        )
        .map_err(|error| error.to_string())?;
    let Some(value) = response.value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    let text = String::from_utf8(value).map_err(|error| error.to_string())?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    let id = Uuid::parse_str(text.trim()).map_err(|error| error.to_string())?;
    Ok(Some(id))
}

fn set_runtime_context_id(
    caller: &impl HostRuntimeApi,
    runtime_state: &TabRuntimeStateHandle,
    key: &str,
    context_id: Option<Uuid>,
) -> Result<(), String> {
    if set_in_memory_runtime_context_id(runtime_state, key, context_id) {
        return Ok(());
    }
    context_id.map_or_else(
        || clear_runtime_context_id(caller, runtime_state, key),
        |context_id| {
            caller
                .call_service::<_, ()>(
                    "bmux.storage",
                    bmux_plugin_sdk::ServiceKind::Command,
                    "volatile-state-command/v1",
                    "set",
                    &VolatileStateSetRequest::new(
                        storage_key(key),
                        context_id.to_string().into_bytes(),
                    ),
                )
                .map_err(|error| error.to_string())
        },
    )
}

fn set_in_memory_runtime_context_id(
    runtime_state: &TabRuntimeStateHandle,
    key: &str,
    context_id: Option<Uuid>,
) -> bool {
    let Ok(mut state) = runtime_state.lock() else {
        return false;
    };
    match key {
        ACTIVE_TAB_CONTEXT_KEY => state.active_context_id = context_id,
        PREVIOUS_TAB_CONTEXT_KEY => state.previous_context_id = context_id,
        _ => return false,
    }
    true
}

fn clear_runtime_context_id(
    caller: &impl HostRuntimeApi,
    runtime_state: &TabRuntimeStateHandle,
    key: &str,
) -> Result<(), String> {
    if set_in_memory_runtime_context_id(runtime_state, key, None) {
        return Ok(());
    }
    caller
        .call_service::<_, ()>(
            "bmux.storage",
            bmux_plugin_sdk::ServiceKind::Command,
            "volatile-state-command/v1",
            "clear",
            &VolatileStateClearRequest::new(storage_key(key)),
        )
        .map_err(|error| error.to_string())
}

fn set_stored_context_id(
    caller: &impl HostRuntimeApi,
    key: &str,
    context_id: Option<Uuid>,
) -> Result<(), String> {
    let value = context_id.map_or_else(Vec::new, |id| id.to_string().into_bytes());
    caller
        .storage_set(&StorageSetRequest::new(storage_key(key), value))
        .map_err(|error| error.to_string())
}

fn order_contexts_for_navigation(
    caller: &(impl HostRuntimeApi + Sync),
    runtime_state: &TabRuntimeStateHandle,
    contexts: Vec<ContextSummary>,
) -> Result<Vec<ContextSummary>, String> {
    let order_ids = resolve_tab_order_ids(caller, runtime_state, &contexts)?;
    let mut by_id = contexts
        .into_iter()
        .map(|context| (context.id, context))
        .collect::<BTreeMap<_, _>>();
    Ok(order_ids
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect())
}

fn resolve_tab_order_ids(
    caller: &impl HostRuntimeApi,
    runtime_state: &TabRuntimeStateHandle,
    contexts: &[ContextSummary],
) -> Result<Vec<Uuid>, String> {
    if let Some(order_ids) = cached_tab_order_ids(runtime_state) {
        return Ok(project_tab_order_ids(order_ids, contexts));
    }
    let workspace_id = contexts
        .first()
        .map_or_else(Uuid::nil, context_workspace_id);
    let mut order_ids = get_stored_tab_order_ids_for_workspace(caller, workspace_id)?;
    if order_ids.is_empty() && !contexts.is_empty() {
        order_ids = contexts.iter().map(|context| context.id).collect();
        order_ids.sort_by_key(uuid::Uuid::as_u128);
        set_stored_tab_order_ids_for_workspace(caller, workspace_id, &order_ids)?;
        if let Ok(mut state) = runtime_state.lock() {
            state.tab_order_ids = Some(order_ids.clone());
            state.tab_order_dirty = false;
        }
        return Ok(order_ids);
    }

    let mut changed = false;

    let mut seen = HashSet::new();
    let mut deduped = Vec::with_capacity(order_ids.len());
    for id in order_ids {
        if seen.insert(id) {
            deduped.push(id);
        } else {
            changed = true;
        }
    }
    order_ids = deduped;

    let context_ids = contexts
        .iter()
        .map(|context| context.id)
        .collect::<HashSet<_>>();
    let retained_len = order_ids.len();
    order_ids.retain(|id| context_ids.contains(id));
    if order_ids.len() != retained_len {
        changed = true;
    }

    if changed {
        set_stored_tab_order_ids_for_workspace(caller, workspace_id, &order_ids)?;
    }

    let order_ids = project_tab_order_ids(order_ids, contexts);
    if let Ok(mut state) = runtime_state.lock() {
        state.tab_order_ids = Some(order_ids.clone());
        state.tab_order_dirty = false;
    }
    Ok(order_ids)
}

fn cached_tab_order_ids(runtime_state: &TabRuntimeStateHandle) -> Option<Vec<Uuid>> {
    runtime_state
        .lock()
        .ok()
        .and_then(|state| state.tab_order_ids.clone())
}

fn seed_known_contexts(runtime_state: &TabRuntimeStateHandle, contexts: &[ContextSummary]) {
    if let Ok(mut state) = runtime_state.lock() {
        for context in contexts {
            state
                .known_contexts
                .insert(context.id, context.name.clone());
        }
    }
}

fn cache_known_context(
    runtime_state: &TabRuntimeStateHandle,
    context_id: Uuid,
    name: Option<String>,
) {
    if let Ok(mut state) = runtime_state.lock() {
        state.known_contexts.insert(context_id, name);
    }
}

fn remove_known_context(runtime_state: &TabRuntimeStateHandle, context_id: Uuid) {
    if let Ok(mut state) = runtime_state.lock() {
        state.known_contexts.remove(&context_id);
    }
}

fn project_tab_order_ids(mut order_ids: Vec<Uuid>, contexts: &[ContextSummary]) -> Vec<Uuid> {
    let context_ids = contexts
        .iter()
        .map(|context| context.id)
        .collect::<HashSet<_>>();
    order_ids.retain(|id| context_ids.contains(id));
    let mut known_ids = order_ids.iter().copied().collect::<HashSet<_>>();
    // Append missing contexts only in the returned projection, never
    // in persisted storage. `contexts` is MRU-first, so persisting
    // this fallback would reintroduce tab-order shuffling on every
    // selection. Creation/close event handlers own durable order
    // mutations; this branch is just a display safety net for contexts
    // that predate the tabs order stream.
    let mut missing = contexts
        .iter()
        .filter(|context| !known_ids.contains(&context.id))
        .map(|context| context.id)
        .collect::<Vec<_>>();
    missing.sort_by_key(uuid::Uuid::as_u128);
    for id in missing {
        if known_ids.insert(id) {
            order_ids.push(id);
        }
    }
    order_ids
}

fn workspace_order_storage_key(workspace_id: Uuid) -> bmux_plugin_sdk::StorageKey {
    storage_key(&format!("tabs.order.{}", workspace_id.simple()))
}

fn get_stored_tab_order_ids_for_workspace(
    caller: &impl HostRuntimeApi,
    workspace_id: Uuid,
) -> Result<Vec<Uuid>, String> {
    let response = caller
        .storage_get(&StorageGetRequest::new(workspace_order_storage_key(
            workspace_id,
        )))
        .map_err(|error| error.to_string())?;
    if let Some(value) = response.value
        && !value.is_empty()
    {
        return parse_stored_tab_order_value(value);
    }
    if workspace_id.is_nil() {
        let legacy = get_stored_tab_order_ids(caller)?;
        if !legacy.is_empty() {
            set_stored_tab_order_ids_for_workspace(caller, workspace_id, &legacy)?;
        }
        return Ok(legacy);
    }
    Ok(Vec::new())
}

fn parse_stored_tab_order_value(value: Vec<u8>) -> Result<Vec<Uuid>, String> {
    if let Ok(raw) = serde_json::from_slice::<Vec<String>>(&value) {
        return parse_stored_tab_order_entries(raw);
    }
    let text = String::from_utf8(value)
        .map_err(|error| format!("failed parsing stored tab order as utf8: {error}"))?;
    parse_stored_tab_order_entries(
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
    )
}

fn set_stored_tab_order_ids_for_workspace(
    caller: &impl HostRuntimeApi,
    workspace_id: Uuid,
    order_ids: &[Uuid],
) -> Result<(), String> {
    caller
        .storage_set(&StorageSetRequest::new(
            workspace_order_storage_key(workspace_id),
            encode_stored_tab_order_lines(order_ids),
        ))
        .map_err(|error| error.to_string())
}

fn get_stored_tab_order_ids(caller: &impl HostRuntimeApi) -> Result<Vec<Uuid>, String> {
    let response = caller
        .storage_get(&StorageGetRequest::new(bmux_plugin_sdk::storage_key!(
            "tabs.order"
        )))
        .map_err(|error| error.to_string())?;
    let Some(value) = response.value else {
        return Ok(Vec::new());
    };
    if value.is_empty() {
        return Ok(Vec::new());
    }
    parse_stored_tab_order_value(value)
}

fn parse_stored_tab_order_entries(raw: Vec<String>) -> Result<Vec<Uuid>, String> {
    raw.into_iter()
        .map(|entry| {
            Uuid::parse_str(entry.trim())
                .map_err(|error| format!("failed parsing stored tab order UUID '{entry}': {error}"))
        })
        .collect()
}

#[cfg(test)]
fn set_stored_tab_order_ids(
    caller: &impl HostRuntimeApi,
    order_ids: &[Uuid],
) -> Result<(), String> {
    let value = encode_stored_tab_order_lines(order_ids);
    caller
        .storage_set(&StorageSetRequest::new(
            bmux_plugin_sdk::storage_key!("tabs.order"),
            value,
        ))
        .map_err(|error| error.to_string())
}

fn encode_stored_tab_order_lines(order_ids: &[Uuid]) -> Vec<u8> {
    if order_ids.is_empty() {
        return Vec::new();
    }
    let mut text = order_ids
        .iter()
        .map(Uuid::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    text.push('\n');
    text.into_bytes()
}

// ── Typed service handles ────────────────────────────────────────────
//
// The BPDL-generated `TabsCommandsService` and `TabsStateService`
// traits are implemented on dedicated handle structs that carry an owned
// `TypedServiceCaller`. The byte-encoded `invoke_service` path remains
// for consumers that don't use typed dispatch; both paths share the
// same underlying sync helpers and the same `LastSelectedByClient` map,
// so behaviour is identical between routes.

/// Shared state backing both the typed commands handle and the byte-
/// encoded dispatch path.
#[derive(Clone)]
struct TabsSharedState {
    caller: Arc<TypedServiceCaller>,
    last_selected_by_client: LastSelectedByClient,
    runtime_state: TabRuntimeStateHandle,
}

/// Typed implementation of [`TabsCommandsService`]. Wraps a
/// [`TypedServiceCaller`] so trait methods can drive host calls
/// directly without a per-call [`NativeServiceContext`].
pub struct TabsCommandsHandle {
    shared: TabsSharedState,
}

impl TabsCommandsHandle {
    const fn new(shared: TabsSharedState) -> Self {
        Self { shared }
    }
}

/// Typed implementation of [`TabsStateService`]. Reads live pane
/// state through the same host runtime the byte path uses.
pub struct TabsStateHandle {
    shared: TabsSharedState,
}

impl TabsStateHandle {
    const fn new(shared: TabsSharedState) -> Self {
        Self { shared }
    }
}

#[allow(clippy::needless_pass_by_value)] // Used as a fn-pointer in `.map_err(...)`; ref-taking would require closures.
fn map_host_error<E: ToString>(err: E) -> PaneMutationError {
    PaneMutationError::Failed {
        reason: err.to_string(),
    }
}

impl TabsCommandsService for TabsCommandsHandle {
    fn focus_pane<'a>(
        &'a self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<(), FocusError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let target = Selector {
                id: Some(id),
                name: None,
                index: None,
            };
            focus_pane(&*caller, None, Some(&target), "")
                .map(|_| ())
                .map_err(|error| FocusError::FocusDenied { reason: error })
        })
    }

    fn close_pane<'a>(
        &'a self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<(), CloseError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let target = Selector {
                id: Some(id),
                name: None,
                index: None,
            };
            close_pane(&*caller, None, Some(&target))
                .map(|_| ())
                .map_err(|error| CloseError::CloseDenied { reason: error })
        })
    }

    fn focus_pane_by_selector<'a>(
        &'a self,
        session: Option<Selector>,
        target: Selector,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            focus_pane(&*caller, session.as_ref(), Some(&target), "")
                .map(|response| PaneAck {
                    ok: true,
                    pane_id: Some(response.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn close_pane_by_selector<'a>(
        &'a self,
        session: Option<Selector>,
        target: Selector,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            close_pane(&*caller, session.as_ref(), Some(&target))
                .map(|response| PaneAck {
                    ok: true,
                    pane_id: Some(response.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn close_active_pane<'a>(
        &'a self,
        session: Option<Selector>,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            close_pane(&*caller, session.as_ref(), None)
                .map(|response| PaneAck {
                    ok: true,
                    pane_id: Some(response.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn focus_pane_in_direction<'a>(
        &'a self,
        session: Option<Selector>,
        direction: PaneDirection,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let Some(focus_dir) = focus_direction_name(direction) else {
                return Err(PaneMutationError::InvalidArgument {
                    reason: "direction must be left/right/up/down".into(),
                });
            };
            focus_pane(&*caller, session.as_ref(), None, focus_dir)
                .map(|response| PaneAck {
                    ok: true,
                    pane_id: Some(response.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn split_pane<'a>(
        &'a self,
        session: Option<Selector>,
        target: Option<Selector>,
        direction: PaneDirection,
        ratio_pct: Option<u32>,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            split_pane(
                &*caller,
                session.as_ref(),
                target.as_ref(),
                direction,
                ratio_pct,
            )
            .map(|response| PaneAck {
                ok: true,
                pane_id: Some(response.pane_id),
            })
            .map_err(map_host_error)
        })
    }

    fn launch_pane<'a>(
        &'a self,
        session: Option<Selector>,
        target: Option<Selector>,
        direction: PaneDirection,
        name: Option<String>,
        program: String,
        args: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            launch_pane(
                &*caller,
                session.as_ref(),
                target.as_ref(),
                LaunchPaneRequest {
                    direction,
                    name,
                    program,
                    args,
                    cwd: None,
                },
            )
            .map(|response| PaneAck {
                ok: true,
                pane_id: Some(response.pane_id),
            })
            .map_err(map_host_error)
        })
    }

    fn resize_pane<'a>(
        &'a self,
        session: Option<Selector>,
        target: Option<Selector>,
        direction: PaneResizeDirection,
        cells: u16,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            resize_pane(
                &*caller,
                session.as_ref(),
                target.as_ref(),
                direction,
                cells,
            )
            .map(|_| PaneAck {
                ok: true,
                pane_id: None,
            })
            .map_err(map_host_error)
        })
    }

    fn zoom_pane<'a>(
        &'a self,
        session: Option<Selector>,
    ) -> Pin<Box<dyn Future<Output = Result<PaneZoomAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            zoom_pane(&*caller, session.as_ref())
                .map(|response| PaneZoomAck {
                    pane_id: response.pane_id,
                    zoomed: true,
                })
                .map_err(map_host_error)
        })
    }

    fn restart_pane<'a>(
        &'a self,
        session: Option<Selector>,
        target: Option<Selector>,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            restart_pane(&*caller, session.as_ref(), target.as_ref())
                .map(|response| PaneAck {
                    ok: true,
                    pane_id: Some(response.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    #[allow(
        clippy::many_single_char_names,
        reason = "Generated BPDL trait uses geometry field names x/y/w/h/z."
    )]
    fn create_floating_pane<'a>(
        &'a self,
        session: Option<Selector>,
        target: Option<Selector>,
        x: Option<u16>,
        y: Option<u16>,
        w: Option<u16>,
        h: Option<u16>,
        z: Option<i32>,
        layer: Option<String>,
        scope: Option<String>,
        program: Option<String>,
        args: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            create_floating_pane(
                &*caller,
                session.as_ref(),
                target.as_ref(),
                FloatingPaneCommandOptions {
                    origin_x: x,
                    origin_y: y,
                    width: w,
                    height: h,
                    z_index: z,
                    layer,
                    scope,
                    program,
                    args,
                },
            )
            .map(|ack| PaneAck {
                ok: true,
                pane_id: Some(ack.pane_id),
            })
            .map_err(map_host_error)
        })
    }

    fn move_floating_pane<'a>(
        &'a self,
        session: Option<Selector>,
        target: Selector,
        x: u16,
        y: u16,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            move_floating_pane(&*caller, session.as_ref(), &target, x, y)
                .map(|ack| PaneAck {
                    ok: true,
                    pane_id: Some(ack.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn resize_floating_pane<'a>(
        &'a self,
        session: Option<Selector>,
        target: Selector,
        w: u16,
        h: u16,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            resize_floating_pane(&*caller, session.as_ref(), &target, w, h)
                .map(|ack| PaneAck {
                    ok: true,
                    pane_id: Some(ack.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn move_active_floating_pane<'a>(
        &'a self,
        session: Option<Selector>,
        direction: FloatingPaneMoveDirection,
        cells: u16,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            move_active_floating_pane(&*caller, session.as_ref(), direction, cells)
                .map(|ack| PaneAck {
                    ok: true,
                    pane_id: Some(ack.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn resize_active_floating_pane<'a>(
        &'a self,
        session: Option<Selector>,
        direction: PaneResizeDirection,
        cells: u16,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            resize_active_floating_pane(&*caller, session.as_ref(), direction, cells)
                .map(|ack| PaneAck {
                    ok: true,
                    pane_id: Some(ack.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn focus_floating_pane<'a>(
        &'a self,
        session: Option<Selector>,
        target: Selector,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            mutate_floating_pane(&*caller, session.as_ref(), &target, "focus")
                .map(|ack| PaneAck {
                    ok: true,
                    pane_id: Some(ack.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn raise_floating_pane<'a>(
        &'a self,
        session: Option<Selector>,
        target: Selector,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            mutate_floating_pane(&*caller, session.as_ref(), &target, "raise")
                .map(|ack| PaneAck {
                    ok: true,
                    pane_id: Some(ack.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn lower_floating_pane<'a>(
        &'a self,
        session: Option<Selector>,
        target: Selector,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            mutate_floating_pane(&*caller, session.as_ref(), &target, "lower")
                .map(|ack| PaneAck {
                    ok: true,
                    pane_id: Some(ack.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn close_floating_pane<'a>(
        &'a self,
        session: Option<Selector>,
        target: Selector,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            mutate_floating_pane(&*caller, session.as_ref(), &target, "close")
                .map(|ack| PaneAck {
                    ok: true,
                    pane_id: Some(ack.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn focus_next_floating_pane<'a>(
        &'a self,
        session: Option<Selector>,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            focus_next_floating_pane(&*caller, session.as_ref())
                .map(|ack| PaneAck {
                    ok: true,
                    pane_id: Some(ack.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn raise_active_floating_pane<'a>(
        &'a self,
        session: Option<Selector>,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            mutate_active_floating_pane(&*caller, session.as_ref(), "raise")
                .map(|ack| PaneAck {
                    ok: true,
                    pane_id: Some(ack.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn lower_active_floating_pane<'a>(
        &'a self,
        session: Option<Selector>,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            mutate_active_floating_pane(&*caller, session.as_ref(), "lower")
                .map(|ack| PaneAck {
                    ok: true,
                    pane_id: Some(ack.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn close_active_floating_pane<'a>(
        &'a self,
        session: Option<Selector>,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            mutate_active_floating_pane(&*caller, session.as_ref(), "close")
                .map(|ack| PaneAck {
                    ok: true,
                    pane_id: Some(ack.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn set_floating_pane_z<'a>(
        &'a self,
        session: Option<Selector>,
        target: Selector,
        z: i32,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            set_floating_pane_z(&*caller, session.as_ref(), &target, z)
                .map(|ack| PaneAck {
                    ok: true,
                    pane_id: Some(ack.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn set_floating_pane_layer<'a>(
        &'a self,
        session: Option<Selector>,
        target: Selector,
        layer: String,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            set_floating_pane_layer(&*caller, session.as_ref(), &target, layer)
                .map(|ack| PaneAck {
                    ok: true,
                    pane_id: Some(ack.pane_id),
                })
                .map_err(map_host_error)
        })
    }

    fn new_tab<'a>(
        &'a self,
        name: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<TabAck, TabError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            create_tab(&*caller, &self.shared.runtime_state, name)
                .map_err(|reason| TabError::Failed { reason })
        })
    }

    fn rename_tab<'a>(
        &'a self,
        name: String,
    ) -> Pin<Box<dyn Future<Output = Result<TabAck, TabError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            rename_tab(&*caller, &self.shared.runtime_state, &name)
                .map_err(|reason| TabError::Failed { reason })
        })
    }

    fn rename_tab_by_id<'a>(
        &'a self,
        id: Uuid,
        name: String,
    ) -> Pin<Box<dyn Future<Output = Result<TabAck, TabError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            rename_tab_by_id(&*caller, &self.shared.runtime_state, id, &name)
                .map_err(|reason| TabError::Failed { reason })
        })
    }

    fn kill_tab<'a>(
        &'a self,
        target: String,
        force_local: bool,
    ) -> Pin<Box<dyn Future<Output = Result<TabAck, TabError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let selector = parse_selector(&target).map_err(|reason| TabError::Failed { reason })?;
            kill_tab(&*caller, &self.shared.runtime_state, selector, force_local)
                .map_err(|reason| TabError::Failed { reason })
        })
    }

    fn kill_all_tabs<'a>(
        &'a self,
        force_local: bool,
    ) -> Pin<Box<dyn Future<Output = Result<TabAck, TabError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            kill_all_tabs(&*caller, &self.shared.runtime_state, force_local)
                .map_err(|reason| TabError::Failed { reason })
        })
    }

    fn switch_tab<'a>(
        &'a self,
        target: String,
    ) -> Pin<Box<dyn Future<Output = Result<TabAck, TabError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        let last_selected = self.shared.last_selected_by_client.clone();
        Box::pin(async move {
            let selector = parse_selector(&target).map_err(|reason| TabError::Failed { reason })?;
            switch_tab(
                &*caller,
                &self.shared.runtime_state,
                selector,
                &last_selected,
                None,
            )
            .map_err(|reason| TabError::Failed { reason })
        })
    }

    fn move_tab<'a>(
        &'a self,
        source: Uuid,
        target: Uuid,
        placement: TabMovePlacement,
    ) -> Pin<Box<dyn Future<Output = Result<TabAck, TabError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            move_tab(
                &*caller,
                &self.shared.runtime_state,
                source,
                target,
                placement,
            )
            .map_err(|reason| TabError::Failed { reason })
        })
    }
}

/// Read the zoomed pane for `session` from the pane-runtime plugin's
/// `pane-runtime-focus` state channel.
///
/// The channel is a state (watch) channel, so the current value is
/// available synchronously without a service round-trip. Returns `None`
/// when the pane-runtime plugin hasn't registered the channel yet or
/// the session isn't zoomed.
fn zoomed_pane_id_for_session(session: Uuid) -> Option<Uuid> {
    let (snapshot, _rx) = bmux_plugin::global_event_bus()
        .subscribe_state::<bmux_pane_runtime_plugin_api::pane_runtime_focus::SessionFocusStateMap>(
            &bmux_pane_runtime_plugin_api::pane_runtime_focus::STATE_KIND,
        )
        .ok()?;
    snapshot.entries.get(&session)?.zoomed_pane_id
}

impl TabsStateService for TabsStateHandle {
    fn pane_state<'a>(
        &'a self,
        _id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Option<PaneState>> + Send + 'a>> {
        // Pane-level state hasn't been wired yet; return `None` for now
        // and revisit when the scene surfaces enough for the plugin to
        // materialize a full `PaneState` without the host-runtime API
        // exposing pane metadata.
        Box::pin(async move { None })
    }

    fn focused_pane<'a>(
        &'a self,
        _session: Uuid,
    ) -> Pin<Box<dyn Future<Output = Option<Uuid>> + Send + 'a>> {
        Box::pin(async move { None })
    }

    fn zoomed_pane<'a>(
        &'a self,
        session: Uuid,
    ) -> Pin<Box<dyn Future<Output = Option<Uuid>> + Send + 'a>> {
        Box::pin(async move { zoomed_pane_id_for_session(session) })
    }

    fn list_panes<'a>(
        &'a self,
        session: Uuid,
    ) -> Pin<Box<dyn Future<Output = Vec<PaneState>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let Ok(response) = list_panes(&*caller, Some(session)) else {
                return Vec::new();
            };
            let zoomed_pane_id = zoomed_pane_id_for_session(session);
            response
                .panes
                .into_iter()
                .map(|pane| PaneState {
                    id: pane.id,
                    session_id: session,
                    focused: pane.focused,
                    zoomed: zoomed_pane_id == Some(pane.id),
                    name: pane.name,
                    status: tabs_state::PaneStatus::default(),
                })
                .collect()
        })
    }

    fn list_floating_panes<'a>(
        &'a self,
        session: Option<Uuid>,
    ) -> Pin<Box<dyn Future<Output = Vec<FloatingPaneState>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let Ok(panes) = list_floating_panes(&*caller, session) else {
                return Vec::new();
            };
            panes
                .into_iter()
                .map(|(session_id, pane)| FloatingPaneState {
                    id: pane.id,
                    pane_id: pane.pane_id,
                    session_id,
                    anchor_pane_id: pane.anchor_pane_id,
                    context_id: pane.context_id,
                    client_id: pane.client_id,
                    x: pane.x,
                    y: pane.y,
                    w: pane.w,
                    h: pane.h,
                    z: pane.z,
                    layer: pane.layer,
                    scope: pane.scope,
                    visible: pane.visible,
                })
                .collect()
        })
    }

    fn list_tabs<'a>(
        &'a self,
        session: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Vec<TabEntry>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            list_tabs(&*caller, &self.shared.runtime_state, session.as_deref()).unwrap_or_default()
        })
    }

    fn active_tab_panes<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<ActiveTabPaneSet, ActiveTabPaneQueryError>> + Send + 'a>>
    {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move { active_tab_panes(&*caller) })
    }
}

#[cfg(test)]
#[allow(clippy::needless_pass_by_value)] // Test helper; owned selector from deserialized request
fn resolve_context_selector_id(
    caller: &(impl HostRuntimeApi + Sync),
    selector: ContextSelector,
) -> Result<Uuid, String> {
    let contexts = list_contexts(caller)?;
    resolve_context_id_from_contexts(&contexts, &selector)
}

fn parse_selector(value: &str) -> Result<ContextSelector, String> {
    if let Ok(id) = Uuid::parse_str(value) {
        return Ok(context_selector_by_id(id));
    }
    if value.trim().is_empty() {
        return Err("target must not be empty".to_string());
    }
    Ok(ContextSelector {
        id: None,
        name: Some(value.to_string()),
    })
}

fn option_value(arguments: &[String], long_name: &str) -> Option<String> {
    let long_flag = format!("--{long_name}");
    arguments
        .windows(2)
        .find_map(|chunk| (chunk[0] == long_flag).then(|| chunk[1].clone()))
}

fn has_flag(arguments: &[String], long_name: &str) -> bool {
    let long_flag = format!("--{long_name}");
    arguments.iter().any(|argument| argument == &long_flag)
}

fn positional_value(arguments: &[String]) -> Option<String> {
    positional_value_at(arguments, 0)
}

fn positional_value_at(arguments: &[String], position: usize) -> Option<String> {
    arguments
        .iter()
        .filter(|argument| !argument.starts_with('-'))
        .nth(position)
        .cloned()
}

fn parse_tab_move_placement_arg(value: &str) -> Result<TabMovePlacement, String> {
    match value.to_ascii_lowercase().as_str() {
        "before" => Ok(TabMovePlacement::Before),
        "after" => Ok(TabMovePlacement::After),
        other => Err(format!(
            "unknown move placement '{other}' (expected before/after)"
        )),
    }
}

/// Parse a `--direction` argument value from a keybinding-dispatched
/// plugin command into the `PaneDirection` enum understood by the
/// pane-runtime service requests.
///
/// `next` folds to `Right` and `prev`/`previous` fold to `Left` so
/// that `focus_direction_name` emits the correct `next`/`prev`
/// mapping at the pane-runtime boundary.
fn parse_pane_direction_arg(value: &str) -> Result<PaneDirection, String> {
    match value.to_ascii_lowercase().as_str() {
        "horizontal" => Ok(PaneDirection::Horizontal),
        "vertical" => Ok(PaneDirection::Vertical),
        "left" | "prev" | "previous" => Ok(PaneDirection::Left),
        "right" | "next" => Ok(PaneDirection::Right),
        "up" => Ok(PaneDirection::Up),
        "down" => Ok(PaneDirection::Down),
        other => Err(format!("unknown direction '{other}'")),
    }
}

fn parse_pane_resize_direction_arg(value: &str) -> Result<PaneResizeDirection, String> {
    match value.to_ascii_lowercase().as_str() {
        "increase" => Ok(PaneResizeDirection::Increase),
        "decrease" => Ok(PaneResizeDirection::Decrease),
        "left" => Ok(PaneResizeDirection::Left),
        "right" => Ok(PaneResizeDirection::Right),
        "up" => Ok(PaneResizeDirection::Up),
        "down" => Ok(PaneResizeDirection::Down),
        other => Err(format!(
            "unknown resize direction '{other}' (expected increase/decrease/left/right/up/down)"
        )),
    }
}

fn parse_floating_move_direction_arg(value: &str) -> Result<FloatingPaneMoveDirection, String> {
    match value.to_ascii_lowercase().as_str() {
        "left" => Ok(FloatingPaneMoveDirection::Left),
        "right" => Ok(FloatingPaneMoveDirection::Right),
        "up" => Ok(FloatingPaneMoveDirection::Up),
        "down" => Ok(FloatingPaneMoveDirection::Down),
        other => Err(format!(
            "unknown floating pane move direction '{other}' (expected left/right/up/down)"
        )),
    }
}

fn parse_u16_arg(value: &str, name: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .map_err(|_| format!("invalid --{name} value '{value}'"))
}

fn parse_i32_arg(value: &str, name: &str) -> Result<i32, String> {
    value
        .parse::<i32>()
        .map_err(|_| format!("invalid --{name} value '{value}'"))
}

fn parse_pane_id_argument(arguments: &[String]) -> Result<Uuid, String> {
    let raw = option_value(arguments, "pane-id")
        .or_else(|| option_value(arguments, "pane"))
        .or_else(|| positional_value(arguments))
        .ok_or_else(|| "missing floating pane id (pass --pane-id <uuid>)".to_string())?;
    raw.parse::<Uuid>()
        .map_err(|_| format!("invalid pane id '{raw}'"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListTabsArgs {
    session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NewTabArgs {
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RenameTabArgs {
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RenameTabByIdArgs {
    id: Uuid,
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KillTabArgs {
    target: String,
    force_local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KillAllTabsArgs {
    force_local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SwitchTabArgs {
    target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MoveTabArgs {
    source: Uuid,
    target: Uuid,
    placement: TabMovePlacement,
}

/// Byte-wire envelope for `tabs-commands/focus-pane`. The BPDL
/// trait's `focus_pane(id: uuid)` parameters serialize as a JSON
/// object with a single `id` field at the wire boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FocusPaneArgs {
    id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClosePaneArgs {
    id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FocusPaneBySelectorArgs {
    #[serde(default)]
    session: Option<Selector>,
    target: Selector,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClosePaneBySelectorArgs {
    #[serde(default)]
    session: Option<Selector>,
    target: Selector,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CloseActivePaneArgs {
    #[serde(default)]
    session: Option<Selector>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FocusPaneInDirectionArgs {
    #[serde(default)]
    session: Option<Selector>,
    direction: PaneDirection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SplitPaneArgs {
    #[serde(default)]
    session: Option<Selector>,
    #[serde(default)]
    target: Option<Selector>,
    direction: PaneDirection,
    #[serde(default)]
    ratio_pct: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LaunchPaneArgs {
    #[serde(default)]
    session: Option<Selector>,
    #[serde(default)]
    target: Option<Selector>,
    direction: PaneDirection,
    #[serde(default)]
    name: Option<String>,
    program: String,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ResizePaneArgs {
    #[serde(default)]
    session: Option<Selector>,
    #[serde(default)]
    target: Option<Selector>,
    direction: PaneResizeDirection,
    #[serde(default = "default_resize_cells")]
    cells: u16,
}

const fn default_resize_cells() -> u16 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MoveFloatingPaneArgs {
    #[serde(default)]
    session: Option<Selector>,
    target: Selector,
    x: u16,
    y: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ZoomPaneArgs {
    #[serde(default)]
    session: Option<Selector>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RestartPaneArgs {
    #[serde(default)]
    session: Option<Selector>,
    #[serde(default)]
    target: Option<Selector>,
}

bmux_plugin_sdk::export_plugin!(TabsPlugin, include_str!("../plugin.toml"));

// Compile-time guards: ensure the string literals used in `route_service!`
// and `plugin.toml` stay in sync with the BPDL-declared interface ids.
// Runtime assertion (executed once at the top of the test suite) that
// the BPDL-generated interface ids exactly match the canonical strings
// the plugin manifest and typed-service dispatch expect. A regression
// in either side will surface immediately.
#[cfg(test)]
#[test]
fn interface_ids_match_bpdl_constants() {
    assert_eq!(tabs_state::INTERFACE_ID.as_str(), "tabs-state");
    assert_eq!(tabs_commands::INTERFACE_ID.as_str(), "tabs-commands");
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_contexts_plugin_api::contexts_state::ContextSummary as SessionSummary;
    use bmux_plugin::ServiceCaller;
    use bmux_plugin_sdk::{
        ApiVersion, HostConnectionInfo, HostKernelBridge, HostMetadata, HostScope,
        NativeServiceContext, ProviderId, RegisteredService, ServiceKind, ServiceRequest,
        decode_service_message, encode_service_message,
    };
    use std::sync::Mutex;

    #[derive(Debug, Clone)]
    struct ContextCloseRequest {
        selector: ContextSelector,
        force: bool,
    }

    fn selector_by_name(name: &str) -> ContextSelector {
        ContextSelector {
            id: None,
            name: Some(name.to_string()),
        }
    }

    #[test]
    fn active_tab_pane_set_preserves_tab_session_and_pane_identity() {
        let tab_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let tab = ContextSummary {
            id: tab_id,
            name: Some("work".to_string()),
            attributes: BTreeMap::new(),
        };
        let panes = api_pane_runtime_state::SessionPaneList {
            session_id,
            panes: vec![
                api_pane_runtime_state::PaneSummary {
                    id: first,
                    name: None,
                    shell: "sh".to_string(),
                    active_command: None,
                    focused: true,
                },
                api_pane_runtime_state::PaneSummary {
                    id: second,
                    name: None,
                    shell: "sh".to_string(),
                    active_command: None,
                    focused: false,
                },
            ],
        };

        let result = active_tab_pane_set(&tab, Some(session_id), panes).unwrap();

        assert_eq!(result.tab_id, tab_id);
        assert_eq!(result.session_id, session_id);
        assert_eq!(result.pane_ids, vec![first, second]);
    }

    #[test]
    fn active_tab_pane_set_rejects_missing_or_mismatched_session() {
        let session_id = Uuid::new_v4();
        let tab = ContextSummary {
            id: Uuid::new_v4(),
            name: None,
            attributes: BTreeMap::new(),
        };
        let panes = api_pane_runtime_state::SessionPaneList {
            session_id,
            panes: Vec::new(),
        };

        assert_eq!(
            active_tab_pane_set(&tab, None, panes.clone()),
            Err(ActiveTabPaneQueryError::NoSelectedSession)
        );
        assert!(matches!(
            active_tab_pane_set(&tab, Some(Uuid::new_v4()), panes),
            Err(ActiveTabPaneQueryError::Failed { .. })
        ));
    }

    #[test]
    fn focus_direction_name_maps_spatial_directions_to_runtime_cycle() {
        assert_eq!(focus_direction_name(PaneDirection::Left), Some("prev"));
        assert_eq!(focus_direction_name(PaneDirection::Up), Some("prev"));
        assert_eq!(focus_direction_name(PaneDirection::Right), Some("next"));
        assert_eq!(focus_direction_name(PaneDirection::Down), Some("next"));
        assert_eq!(focus_direction_name(PaneDirection::Horizontal), None);
        assert_eq!(focus_direction_name(PaneDirection::Vertical), None);
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct BridgeRequest {
        payload: Vec<u8>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct BridgeResponse {
        payload: Vec<u8>,
    }

    #[allow(clippy::too_many_lines)]
    unsafe extern "C" fn service_test_kernel_bridge(
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
    ) -> i32 {
        let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
        let bridge_request: BridgeRequest = match decode_service_message(input) {
            Ok(request) => request,
            Err(_) => return 1,
        };
        let _request: bmux_ipc::Request = match bmux_ipc::decode(&bridge_request.payload) {
            Ok(request) => request,
            Err(_) => return 1,
        };

        let response = bmux_ipc::Response::Err(bmux_ipc::ErrorResponse {
            code: bmux_ipc::ErrorCode::InvalidRequest,
            message: "unsupported request in service bridge test".to_string(),
        });

        let Ok(encoded) = bmux_ipc::encode(&response) else {
            return 1;
        };
        let Ok(output) = encode_service_message(&BridgeResponse { payload: encoded }) else {
            return 1;
        };

        if output.len() > output_capacity {
            unsafe {
                *output_len = output.len();
            }
            return 4;
        }

        unsafe {
            std::ptr::copy_nonoverlapping(output.as_ptr(), output_ptr, output.len());
            *output_len = output.len();
        }
        0
    }

    /// Install a thread-local router that answers the typed cross-
    /// plugin service calls tabs-plugin makes through
    /// `KernelOps`'s context/session helpers. Tests that exercise
    /// `invoke_service`-style service dispatch keep the returned
    /// guard alive for the duration of the test.
    ///
    /// The router captures `deny_close`/`deny_create` flags the tests
    /// use to simulate the contexts plugin rejecting a command.
    #[allow(
        clippy::too_many_lines,
        clippy::result_large_err,
        clippy::items_after_statements,
        clippy::redundant_clone
    )]
    fn install_context_test_router(
        deny_create: bool,
        deny_close: bool,
    ) -> bmux_plugin::test_support::TestServiceRouterGuard {
        use bmux_plugin::test_support::{TestServiceRouter, install_test_service_router};
        let selected = Mutex::new(Uuid::from_u128(1));
        let router: TestServiceRouter = std::sync::Arc::new(
            move |_caller_plugin,
                  _caller_client,
                  _capability,
                  _kind,
                  interface,
                  operation,
                  payload| {
                match (interface, operation) {
                    ("contexts-state", "list-contexts") => {
                        let contexts: Vec<
                            bmux_contexts_plugin_api::contexts_state::ContextSummary,
                        > = ["alpha", "beta"]
                            .into_iter()
                            .enumerate()
                            .map(|(index, name)| {
                                bmux_contexts_plugin_api::contexts_state::ContextSummary {
                                    id: Uuid::from_u128(index as u128 + 1),
                                    name: Some(name.to_string()),
                                    attributes: BTreeMap::new(),
                                }
                            })
                            .collect();
                        encode_service_message(&contexts)
                    }
                    ("contexts-state", "current-context") => {
                        let selected_id = *selected.lock().unwrap();
                        let context: Option<
                            bmux_contexts_plugin_api::contexts_state::ContextSummary,
                        > = Some(bmux_contexts_plugin_api::contexts_state::ContextSummary {
                            id: selected_id,
                            name: Some(
                                if selected_id == Uuid::from_u128(1) {
                                    "alpha"
                                } else {
                                    "beta"
                                }
                                .to_string(),
                            ),
                            attributes: BTreeMap::new(),
                        });
                        encode_service_message(&context)
                    }
                    ("contexts-commands", "create-context") => {
                        if deny_create {
                            let err: Result<
                                bmux_contexts_plugin_api::contexts_commands::ContextAck,
                                bmux_contexts_plugin_api::contexts_commands::CreateContextError,
                            > = Err(
                                bmux_contexts_plugin_api::contexts_commands::CreateContextError::Failed {
                                    reason: "session policy denied for this operation".to_string(),
                                },
                            );
                            return encode_service_message(&err);
                        }
                        #[derive(Deserialize)]
                        struct Args {
                            name: Option<String>,
                            #[serde(default)]
                            #[allow(dead_code)]
                            attributes: BTreeMap<String, String>,
                        }
                        let request: Args = decode_service_message(&payload)?;
                        let name_for_deny = request.name.as_deref();
                        if name_for_deny == Some("deny") {
                            let err: Result<
                                bmux_contexts_plugin_api::contexts_commands::ContextAck,
                                bmux_contexts_plugin_api::contexts_commands::CreateContextError,
                            > = Err(
                                bmux_contexts_plugin_api::contexts_commands::CreateContextError::Failed {
                                    reason: "session policy denied for this operation".to_string(),
                                },
                            );
                            return encode_service_message(&err);
                        }
                        let ok: Result<
                            bmux_contexts_plugin_api::contexts_commands::ContextAck,
                            bmux_contexts_plugin_api::contexts_commands::CreateContextError,
                        > = Ok(bmux_contexts_plugin_api::contexts_commands::ContextAck {
                            id: Uuid::new_v4(),
                            session_id: None,
                        });
                        encode_service_message(&ok)
                    }
                    ("contexts-commands", "close-context") => {
                        if deny_close {
                            let err: Result<
                                bmux_contexts_plugin_api::contexts_commands::ContextAck,
                                bmux_contexts_plugin_api::contexts_commands::CloseContextError,
                            > = Err(
                                bmux_contexts_plugin_api::contexts_commands::CloseContextError::Failed {
                                    reason: "session policy denied for this operation".to_string(),
                                },
                            );
                            return encode_service_message(&err);
                        }
                        let ok: Result<
                            bmux_contexts_plugin_api::contexts_commands::ContextAck,
                            bmux_contexts_plugin_api::contexts_commands::CloseContextError,
                        > = Ok(bmux_contexts_plugin_api::contexts_commands::ContextAck {
                            id: Uuid::new_v4(),
                            session_id: None,
                        });
                        encode_service_message(&ok)
                    }
                    ("contexts-commands", "select-context") => {
                        let request: contexts_commands::client::SelectContextRequest =
                            decode_service_message(&payload)?;
                        let id = request.selector.id.expect("switch resolves a context ID");
                        *selected.lock().unwrap() = id;
                        let ok: Result<
                            bmux_contexts_plugin_api::contexts_commands::ContextAck,
                            bmux_contexts_plugin_api::contexts_commands::SelectContextError,
                        > = Ok(bmux_contexts_plugin_api::contexts_commands::ContextAck {
                            id,
                            session_id: None,
                        });
                        encode_service_message(&ok)
                    }
                    // Storage operations for tests.
                    ("storage-query/v1", "get") => {
                        encode_service_message(&bmux_plugin_sdk::StorageGetResponse { value: None })
                    }
                    ("storage-command/v1", "set")
                    | ("volatile-state-command/v1", "set" | "clear") => encode_service_message(&()),
                    ("volatile-state-query/v1", "get") => {
                        encode_service_message(&bmux_plugin_sdk::VolatileStateGetResponse {
                            value: None,
                        })
                    }
                    _ => Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                        operation: "tabs_test_router",
                    }),
                }
            },
        );
        install_test_service_router(router)
    }

    fn service_test_context(
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
        capability: &str,
        kind: ServiceKind,
    ) -> NativeServiceContext {
        let host_services = vec![
            RegisteredService {
                capability: HostScope::new("bmux.contexts.read").expect("capability should parse"),
                kind: ServiceKind::Query,
                interface_id: "contexts-state".to_string(),
                provider: ProviderId::Host,
            },
            RegisteredService {
                capability: HostScope::new("bmux.contexts.write").expect("capability should parse"),
                kind: ServiceKind::Command,
                interface_id: "contexts-commands".to_string(),
                provider: ProviderId::Host,
            },
            RegisteredService {
                capability: HostScope::new("bmux.clients.read").expect("capability should parse"),
                kind: ServiceKind::Query,
                interface_id: "clients-state".to_string(),
                provider: ProviderId::Host,
            },
            RegisteredService {
                capability: HostScope::new("bmux.storage").expect("capability should parse"),
                kind: ServiceKind::Query,
                interface_id: "storage-query/v1".to_string(),
                provider: ProviderId::Host,
            },
            RegisteredService {
                capability: HostScope::new("bmux.storage").expect("capability should parse"),
                kind: ServiceKind::Command,
                interface_id: "storage-command/v1".to_string(),
                provider: ProviderId::Host,
            },
            RegisteredService {
                capability: HostScope::new("bmux.storage").expect("capability should parse"),
                kind: ServiceKind::Query,
                interface_id: "volatile-state-query/v1".to_string(),
                provider: ProviderId::Host,
            },
            RegisteredService {
                capability: HostScope::new("bmux.storage").expect("capability should parse"),
                kind: ServiceKind::Command,
                interface_id: "volatile-state-command/v1".to_string(),
                provider: ProviderId::Host,
            },
        ];

        NativeServiceContext {
            plugin_id: "bmux.tabs".to_string(),
            request: ServiceRequest {
                caller_plugin_id: "test.caller".to_string(),
                service: RegisteredService {
                    capability: HostScope::new(capability).expect("capability should parse"),
                    kind,
                    interface_id: interface_id.to_string(),
                    provider: ProviderId::Plugin("bmux.tabs".to_string()),
                },
                operation: operation.to_string(),
                payload,
            },
            required_capabilities: vec![
                "bmux.commands".to_string(),
                "bmux.contexts.read".to_string(),
                "bmux.contexts.write".to_string(),
                "bmux.clients.read".to_string(),
                "bmux.storage".to_string(),
            ],
            provided_capabilities: vec![
                "bmux.tabs.read".to_string(),
                "bmux.tabs.write".to_string(),
            ],
            services: host_services,
            available_capabilities: vec![
                "bmux.contexts.read".to_string(),
                "bmux.contexts.write".to_string(),
                "bmux.clients.read".to_string(),
                "bmux.storage".to_string(),
            ],
            enabled_plugins: vec!["bmux.tabs".to_string()],
            plugin_search_roots: vec!["/plugins".to_string()],
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: vec!["/config".to_string()],
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: std::collections::BTreeMap::new(),
            caller_client_id: None,
            cancellation: bmux_plugin_sdk::CancellationToken::default(),
            host_kernel_bridge: Some(HostKernelBridge::from_fn(service_test_kernel_bridge)),
        }
    }

    struct MockHost {
        sessions: Vec<SessionSummary>,
        fail_create: bool,
        fail_kill: bool,
        fail_current_client: bool,
        current_client_id: Uuid,
        selected_session_id: Mutex<Option<Uuid>>,
        mru_context_ids: Mutex<Vec<Uuid>>,
        created_contexts: Mutex<Vec<SessionSummary>>,
        creates: Mutex<Vec<Option<String>>>,
        kills: Mutex<Vec<ContextCloseRequest>>,
        selects: Mutex<Vec<Uuid>>,
        renames: Mutex<Vec<(Uuid, String)>>,
        storage: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl MockHost {
        fn with_sessions(sessions: Vec<SessionSummary>) -> Self {
            Self {
                current_client_id: Uuid::new_v4(),
                selected_session_id: Mutex::new(sessions.first().map(|session| session.id)),
                mru_context_ids: Mutex::new(sessions.iter().map(|session| session.id).collect()),
                created_contexts: Mutex::new(Vec::new()),
                sessions,
                fail_create: false,
                fail_kill: false,
                fail_current_client: false,
                creates: Mutex::new(Vec::new()),
                kills: Mutex::new(Vec::new()),
                selects: Mutex::new(Vec::new()),
                renames: Mutex::new(Vec::new()),
                storage: Mutex::new(BTreeMap::new()),
            }
        }

        fn with_client_query_failure() -> Self {
            let sessions = sample_sessions();
            Self {
                current_client_id: Uuid::new_v4(),
                selected_session_id: Mutex::new(sessions.first().map(|session| session.id)),
                mru_context_ids: Mutex::new(sessions.iter().map(|session| session.id).collect()),
                created_contexts: Mutex::new(Vec::new()),
                sessions,
                fail_create: false,
                fail_kill: false,
                fail_current_client: true,
                creates: Mutex::new(Vec::new()),
                kills: Mutex::new(Vec::new()),
                selects: Mutex::new(Vec::new()),
                renames: Mutex::new(Vec::new()),
                storage: Mutex::new(BTreeMap::new()),
            }
        }

        fn with_failures(fail_create: bool, fail_kill: bool, _fail_pane_list: bool) -> Self {
            let sessions = sample_sessions();
            Self {
                current_client_id: Uuid::new_v4(),
                selected_session_id: Mutex::new(sessions.first().map(|session| session.id)),
                mru_context_ids: Mutex::new(sessions.iter().map(|session| session.id).collect()),
                created_contexts: Mutex::new(Vec::new()),
                sessions,
                fail_create,
                fail_kill,
                fail_current_client: false,
                creates: Mutex::new(Vec::new()),
                kills: Mutex::new(Vec::new()),
                selects: Mutex::new(Vec::new()),
                renames: Mutex::new(Vec::new()),
                storage: Mutex::new(BTreeMap::new()),
            }
        }
    }

    impl ServiceCaller for MockHost {
        #[allow(
            clippy::too_many_lines,
            clippy::items_after_statements,
            clippy::redundant_clone
        )]
        fn call_service_raw(
            &self,
            _capability: &str,
            _kind: ServiceKind,
            interface_id: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            match (interface_id, operation) {
                // Typed contexts-plugin-api interfaces (the canonical
                // cross-plugin dispatch path used by KernelOps after
                // the `Request::*Context*` IPC variants were retired).
                ("contexts-state", "list-contexts") => {
                    let mru_ids = self
                        .mru_context_ids
                        .lock()
                        .expect("mru context lock should succeed")
                        .clone();
                    let mut all_sessions = self.sessions.clone();
                    all_sessions.extend(
                        self.created_contexts
                            .lock()
                            .expect("created contexts lock should succeed")
                            .iter()
                            .cloned(),
                    );
                    let mut by_id = all_sessions
                        .iter()
                        .cloned()
                        .map(|context| (context.id, context))
                        .collect::<BTreeMap<_, _>>();
                    let mut contexts = Vec::with_capacity(by_id.len());
                    for context_id in mru_ids {
                        if let Some(context) = by_id.remove(&context_id) {
                            contexts.push(context);
                        }
                    }
                    contexts.extend(by_id.into_values());
                    let typed: Vec<bmux_contexts_plugin_api::contexts_state::ContextSummary> =
                        contexts;
                    encode_service_message(&typed)
                }
                ("contexts-state", "current-context") => {
                    let current_context_id = *self
                        .selected_session_id
                        .lock()
                        .expect("selected context lock should succeed");
                    let typed: Option<bmux_contexts_plugin_api::contexts_state::ContextSummary> =
                        current_context_id.and_then(|id| {
                            self.sessions.iter().find(|entry| entry.id == id).cloned()
                        });
                    encode_service_message(&typed)
                }
                ("contexts-commands", "create-context") => {
                    if self.fail_create {
                        let err: Result<
                            bmux_contexts_plugin_api::contexts_commands::ContextAck,
                            bmux_contexts_plugin_api::contexts_commands::CreateContextError,
                        > = Err(
                            bmux_contexts_plugin_api::contexts_commands::CreateContextError::Failed {
                                reason: "mock create failure".to_string(),
                            },
                        );
                        return encode_service_message(&err);
                    }
                    #[derive(Deserialize)]
                    struct Args {
                        name: Option<String>,
                        #[serde(default)]
                        #[allow(dead_code)]
                        attributes: BTreeMap<String, String>,
                    }
                    let request: Args = decode_service_message(&payload)?;
                    self.creates
                        .lock()
                        .expect("create log lock should succeed")
                        .push(request.name.clone());
                    let created_id = Uuid::new_v4();
                    self.created_contexts
                        .lock()
                        .expect("created contexts lock should succeed")
                        .push(SessionSummary {
                            id: created_id,
                            name: request.name.clone(),
                            attributes: BTreeMap::new(),
                        });
                    {
                        let mut mru_context_ids = self
                            .mru_context_ids
                            .lock()
                            .expect("mru context lock should succeed");
                        mru_context_ids.retain(|id| *id != created_id);
                        mru_context_ids.insert(0, created_id);
                    }
                    *self
                        .selected_session_id
                        .lock()
                        .expect("selected session lock should succeed") = Some(created_id);
                    let ok: Result<
                        bmux_contexts_plugin_api::contexts_commands::ContextAck,
                        bmux_contexts_plugin_api::contexts_commands::CreateContextError,
                    > = Ok(bmux_contexts_plugin_api::contexts_commands::ContextAck {
                        id: created_id,
                        session_id: None,
                    });
                    encode_service_message(&ok)
                }
                ("contexts-commands", "close-context") => {
                    if self.fail_kill {
                        let err: Result<
                            bmux_contexts_plugin_api::contexts_commands::ContextAck,
                            bmux_contexts_plugin_api::contexts_commands::CloseContextError,
                        > = Err(
                            bmux_contexts_plugin_api::contexts_commands::CloseContextError::Failed {
                                reason: "mock kill failure".to_string(),
                            },
                        );
                        return encode_service_message(&err);
                    }
                    #[derive(Deserialize)]
                    struct SelectorPayload {
                        id: Option<Uuid>,
                        name: Option<String>,
                    }
                    #[derive(Deserialize)]
                    struct Args {
                        selector: SelectorPayload,
                        #[serde(default)]
                        force: bool,
                    }
                    let request: Args = decode_service_message(&payload)?;
                    let resolved_id = request
                        .selector
                        .id
                        .or_else(|| {
                            request.selector.name.as_ref().and_then(|name| {
                                self.sessions
                                    .iter()
                                    .find(|session| session.name.as_deref() == Some(name.as_str()))
                                    .map(|session| session.id)
                            })
                        })
                        .unwrap_or_else(Uuid::new_v4);
                    self.kills
                        .lock()
                        .expect("kill log lock should succeed")
                        .push(ContextCloseRequest {
                            selector: request
                                .selector
                                .id
                                .map(context_selector_by_id)
                                .or_else(|| {
                                    request.selector.name.clone().map(|name| ContextSelector {
                                        id: None,
                                        name: Some(name),
                                    })
                                })
                                .unwrap_or_else(|| context_selector_by_id(resolved_id)),
                            force: request.force,
                        });
                    let ok: Result<
                        bmux_contexts_plugin_api::contexts_commands::ContextAck,
                        bmux_contexts_plugin_api::contexts_commands::CloseContextError,
                    > = Ok(bmux_contexts_plugin_api::contexts_commands::ContextAck {
                        id: resolved_id,
                        session_id: None,
                    });
                    encode_service_message(&ok)
                }
                ("contexts-commands", "rename-context") => {
                    #[derive(Deserialize)]
                    struct RenameSelectorPayload {
                        id: Option<Uuid>,
                        name: Option<String>,
                    }
                    #[derive(Deserialize)]
                    struct RenameArgs {
                        selector: RenameSelectorPayload,
                        name: String,
                    }
                    let request: RenameArgs = decode_service_message(&payload)?;
                    let id = match (request.selector.id, request.selector.name.as_ref()) {
                        (Some(id), _) => id,
                        (None, Some(name)) => self
                            .sessions
                            .iter()
                            .find(|session| session.name.as_deref() == Some(name.as_str()))
                            .map(|session| session.id)
                            .ok_or_else(|| bmux_plugin_sdk::PluginError::ServiceProtocol {
                                details: "mock rename target not found".to_string(),
                            })?,
                        (None, None) => {
                            return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                                details: "mock rename missing selector".to_string(),
                            });
                        }
                    };
                    self.renames
                        .lock()
                        .expect("renames lock should succeed")
                        .push((id, request.name.clone()));
                    let ok: Result<
                        bmux_contexts_plugin_api::contexts_commands::ContextAck,
                        bmux_contexts_plugin_api::contexts_commands::RenameContextError,
                    > = Ok(bmux_contexts_plugin_api::contexts_commands::ContextAck {
                        id,
                        session_id: None,
                    });
                    encode_service_message(&ok)
                }
                ("contexts-commands", "select-context") => {
                    if self.fail_kill {
                        let err: Result<
                            bmux_contexts_plugin_api::contexts_commands::ContextAck,
                            bmux_contexts_plugin_api::contexts_commands::SelectContextError,
                        > = Err(
                            bmux_contexts_plugin_api::contexts_commands::SelectContextError::Denied {
                                reason: "mock select failure".to_string(),
                            },
                        );
                        return encode_service_message(&err);
                    }
                    #[derive(Deserialize)]
                    struct SelectorPayload {
                        id: Option<Uuid>,
                        name: Option<String>,
                    }
                    #[derive(Deserialize)]
                    struct Args {
                        selector: SelectorPayload,
                    }
                    let request: Args = decode_service_message(&payload)?;
                    let selected = match (request.selector.id, request.selector.name.as_ref()) {
                        (Some(id), _) => {
                            let exists = self.sessions.iter().any(|session| session.id == id)
                                || self
                                    .created_contexts
                                    .lock()
                                    .expect("created contexts lock should succeed")
                                    .iter()
                                    .any(|context| context.id == id);
                            if !exists {
                                return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                                    details: "mock select target not found".to_string(),
                                });
                            }
                            id
                        }
                        (None, Some(name)) => self
                            .sessions
                            .iter()
                            .find(|session| session.name.as_deref() == Some(name.as_str()))
                            .map(|session| session.id)
                            .ok_or_else(|| bmux_plugin_sdk::PluginError::ServiceProtocol {
                                details: "mock select target not found".to_string(),
                            })?,
                        (None, None) => {
                            return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                                details: "mock select missing selector".to_string(),
                            });
                        }
                    };
                    *self
                        .selected_session_id
                        .lock()
                        .expect("selected session lock should succeed") = Some(selected);
                    {
                        let mut mru_context_ids = self
                            .mru_context_ids
                            .lock()
                            .expect("mru context lock should succeed");
                        mru_context_ids.retain(|id| *id != selected);
                        mru_context_ids.insert(0, selected);
                    }
                    self.selects
                        .lock()
                        .expect("select log lock should succeed")
                        .push(selected);
                    let ok: Result<
                        bmux_contexts_plugin_api::contexts_commands::ContextAck,
                        bmux_contexts_plugin_api::contexts_commands::SelectContextError,
                    > = Ok(bmux_contexts_plugin_api::contexts_commands::ContextAck {
                        id: selected,
                        session_id: None,
                    });
                    encode_service_message(&ok)
                }
                ("clients-state", "current-client") => {
                    if self.fail_current_client {
                        return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                            details: "mock current client failure".to_string(),
                        });
                    }
                    let selected_session_id = *self
                        .selected_session_id
                        .lock()
                        .expect("selected session lock should succeed");
                    let summary = bmux_clients_plugin_api::clients_state::ClientSummary {
                        id: self.current_client_id,
                        selected_session_id,
                        selected_context_id: None,
                        following_client_id: None,
                        following_global: false,
                    };
                    let result: Result<
                        bmux_clients_plugin_api::clients_state::ClientSummary,
                        bmux_clients_plugin_api::clients_state::ClientQueryError,
                    > = Ok(summary);
                    encode_service_message(&result)
                }
                ("storage-query/v1", "get") => {
                    let request: StorageGetRequest = decode_service_message(&payload)?;
                    let value = self
                        .storage
                        .lock()
                        .expect("storage lock should succeed")
                        .get(request.key.as_str())
                        .cloned();
                    encode_service_message(&bmux_plugin_sdk::StorageGetResponse { value })
                }
                ("storage-command/v1", "set") => {
                    let request: StorageSetRequest = decode_service_message(&payload)?;
                    self.storage
                        .lock()
                        .expect("storage lock should succeed")
                        .insert(request.key.to_string(), request.value);
                    encode_service_message(&())
                }
                ("volatile-state-query/v1", "get") => {
                    let request: VolatileStateGetRequest = decode_service_message(&payload)?;
                    let value = self
                        .storage
                        .lock()
                        .expect("storage lock should succeed")
                        .get(request.key.as_str())
                        .cloned();
                    encode_service_message(&bmux_plugin_sdk::VolatileStateGetResponse { value })
                }
                ("volatile-state-command/v1", "set") => {
                    let request: VolatileStateSetRequest = decode_service_message(&payload)?;
                    self.storage
                        .lock()
                        .expect("storage lock should succeed")
                        .insert(request.key.to_string(), request.value);
                    encode_service_message(&())
                }
                ("volatile-state-command/v1", "clear") => {
                    let request: VolatileStateClearRequest = decode_service_message(&payload)?;
                    self.storage
                        .lock()
                        .expect("storage lock should succeed")
                        .remove(request.key.as_str());
                    encode_service_message(&())
                }
                _ => Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                    operation: "mock_service",
                }),
            }
        }

        fn execute_kernel_request(
            &self,
            _request: bmux_ipc::Request,
        ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
            Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                operation: "mock_execute_kernel_request",
            })
        }
    }

    fn sample_sessions() -> Vec<SessionSummary> {
        vec![
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("alpha".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("beta".to_string()),
                attributes: BTreeMap::new(),
            },
        ]
    }

    fn sample_sessions_three() -> Vec<SessionSummary> {
        vec![
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("alpha".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("beta".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("gamma".to_string()),
                attributes: BTreeMap::new(),
            },
        ]
    }

    fn seed_tab_order(host: &MockHost, sessions: &[SessionSummary]) {
        let ids = sessions
            .iter()
            .map(|session| session.id)
            .collect::<Vec<_>>();
        set_stored_tab_order_ids(host, &ids).expect("seed tab order should succeed");
    }

    fn runtime_state() -> TabRuntimeStateHandle {
        TabRuntimeStateHandle::default()
    }

    #[test]
    fn list_tabs_projects_sessions_and_marks_first_active() {
        let sessions = sample_sessions();
        let host = MockHost::with_sessions(sessions.clone());
        let runtime_state = runtime_state();
        seed_tab_order(&host, &sessions);
        let tabs = list_tabs(&host, &runtime_state, None).expect("list should succeed");

        assert_eq!(tabs.len(), 2);
        assert!(tabs[0].active);
        assert!(!tabs[1].active);
        assert_eq!(tabs[0].name, "alpha");
        assert_eq!(tabs[1].name, "beta");
    }

    #[test]
    fn list_tabs_filters_by_session_selector() {
        let sessions = sample_sessions();
        let beta_id = sessions[1].id;
        let host = MockHost::with_sessions(sessions);
        let runtime_state = runtime_state();

        let by_name =
            list_tabs(&host, &runtime_state, Some("beta")).expect("list by name should succeed");
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].name, "beta");

        let by_id = list_tabs(&host, &runtime_state, Some(&beta_id.to_string()))
            .expect("list by id should succeed");
        assert_eq!(by_id.len(), 1);
        assert_eq!(by_id[0].id, beta_id.to_string());
    }

    #[test]
    fn rename_tab_by_id_targets_the_requested_tab() {
        let sessions = sample_sessions_three();
        let host = MockHost::with_sessions(sessions.clone());
        let runtime_state = runtime_state();
        seed_tab_order(&host, &sessions);
        // Current tab is the first; rename the third instead.
        let target = sessions[2].id;

        let ack = rename_tab_by_id(&host, &runtime_state, target, "renamed")
            .expect("rename by id should succeed");

        assert!(ack.ok);
        let renames = host.renames.lock().expect("renames lock").clone();
        assert_eq!(renames.as_slice(), &[(target, "renamed".to_string())]);
    }

    #[test]
    fn rename_tab_by_id_rejects_unknown_tab() {
        let sessions = sample_sessions();
        let host = MockHost::with_sessions(sessions.clone());
        let runtime_state = runtime_state();
        seed_tab_order(&host, &sessions);

        let error = rename_tab_by_id(&host, &runtime_state, Uuid::new_v4(), "nope")
            .expect_err("unknown tab should fail");

        assert!(error.contains("unknown tab"), "{error}");
        assert!(
            host.renames.lock().expect("renames lock").is_empty(),
            "no rename should be issued"
        );
    }

    #[test]
    fn rename_tab_by_id_rejects_blank_names() {
        let sessions = sample_sessions();
        let host = MockHost::with_sessions(sessions.clone());
        let runtime_state = runtime_state();
        seed_tab_order(&host, &sessions);

        for blank in ["", "   ", "\t"] {
            let error = rename_tab_by_id(&host, &runtime_state, sessions[0].id, blank)
                .expect_err("blank name should fail");
            assert!(error.contains("must not be empty"), "{error}");
        }
        assert!(host.renames.lock().expect("renames lock").is_empty());
    }

    #[test]
    fn rename_tab_by_id_trims_surrounding_whitespace() {
        let sessions = sample_sessions();
        let host = MockHost::with_sessions(sessions.clone());
        let runtime_state = runtime_state();
        seed_tab_order(&host, &sessions);

        let _ = rename_tab_by_id(&host, &runtime_state, sessions[1].id, "  spaced  ")
            .expect("rename should succeed");

        let renames = host.renames.lock().expect("renames lock").clone();
        assert_eq!(
            renames.as_slice(),
            &[(sessions[1].id, "spaced".to_string())]
        );
    }

    #[test]
    fn rename_tab_renames_the_current_tab() {
        let sessions = sample_sessions_three();
        let host = MockHost::with_sessions(sessions.clone());
        let runtime_state = runtime_state();
        seed_tab_order(&host, &sessions);

        let _ = rename_tab(&host, &runtime_state, "current").expect("rename should succeed");

        let renames = host.renames.lock().expect("renames lock").clone();
        assert_eq!(renames.len(), 1);
        assert_eq!(renames[0].1, "current");
    }

    #[test]
    fn list_tabs_uses_tab_prefix_for_unnamed_contexts() {
        let sessions = vec![
            SessionSummary {
                id: Uuid::new_v4(),
                name: None,
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: Uuid::new_v4(),
                name: None,
                attributes: BTreeMap::new(),
            },
        ];
        let host = MockHost::with_sessions(sessions.clone());
        let runtime_state = runtime_state();
        seed_tab_order(&host, &sessions);

        let tabs = list_tabs(&host, &runtime_state, None).expect("list should succeed");
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].name, "tab-1");
        assert_eq!(tabs[1].name, "tab-2");
    }

    #[test]
    fn resolve_session_id_finds_name_and_id() {
        let sessions = sample_sessions();
        let alpha_id = sessions[0].id;
        let host = MockHost::with_sessions(sessions);

        let resolved_name = resolve_context_selector_id(&host, selector_by_name("alpha"))
            .expect("resolve by name should succeed");
        assert_eq!(resolved_name, alpha_id);

        let resolved_id = resolve_context_selector_id(&host, context_selector_by_id(alpha_id))
            .expect("resolve by id should succeed");
        assert_eq!(resolved_id, alpha_id);
    }

    #[test]
    fn parse_selector_rejects_blank_values() {
        let error = parse_selector("   ").expect_err("blank selector should fail");
        assert!(error.contains("must not be empty"));
    }

    #[test]
    fn normalize_tab_name_rejects_blank_values() {
        let error = normalize_tab_name("  \t  ").expect_err("blank names should be rejected");
        assert!(error.contains("must not be empty"));
    }

    #[test]
    fn create_tab_calls_session_create() {
        let sessions = sample_sessions();
        let first_id = sessions[0].id;
        let host = MockHost::with_sessions(sessions);
        let runtime_state = runtime_state();
        let ack = create_tab(&host, &runtime_state, Some("dev".to_string()))
            .expect("create should succeed");
        assert!(ack.ok);
        let created_id = ack.id.expect("create should return context id");
        let created_id = Uuid::parse_str(&created_id).expect("created id should be uuid");
        let cached_order =
            cached_tab_order_ids(&runtime_state).expect("order cache should be warm");
        assert_eq!(cached_order, vec![first_id, created_id]);
        let creates: Vec<_> = host
            .creates
            .lock()
            .expect("create log lock should succeed")
            .clone();
        assert_eq!(creates.as_slice(), &[Some("dev".to_string())]);
    }

    #[test]
    fn create_tab_seeds_current_context_before_new_context() {
        let sessions = sample_sessions();
        let first_id = sessions[0].id;
        let host = MockHost::with_sessions(sessions);
        let runtime_state = runtime_state();

        let ack = create_tab(&host, &runtime_state, None).expect("create should succeed");
        let created_id =
            Uuid::parse_str(ack.id.as_deref().expect("create should return context id"))
                .expect("created id should be uuid");

        let cached_order =
            cached_tab_order_ids(&runtime_state).expect("order cache should be warm");
        assert_eq!(cached_order, vec![first_id, created_id]);

        let tabs = list_tabs(&host, &runtime_state, None).expect("list should succeed");
        let ids = tabs.iter().map(|tab| tab.id.as_str()).collect::<Vec<_>>();
        let first_text = first_id.to_string();
        let created_text = created_id.to_string();
        assert_eq!(ids[0], first_text.as_str());
        assert_eq!(ids[1], created_text.as_str());
    }

    #[test]
    fn create_tab_assigns_next_tab_name_when_name_is_missing() {
        let sessions = vec![
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("tab-1".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("tab-3".to_string()),
                attributes: BTreeMap::new(),
            },
        ];
        let host = MockHost::with_sessions(sessions);
        let runtime_state = runtime_state();

        let ack = create_tab(&host, &runtime_state, None).expect("create should succeed");
        assert!(ack.ok);
        assert!(ack.id.is_some());
        let creates: Vec<_> = host
            .creates
            .lock()
            .expect("create log lock should succeed")
            .clone();
        assert_eq!(creates.as_slice(), &[Some("tab-2".to_string())]);
    }

    #[test]
    fn kill_all_tabs_calls_kill_for_each_session() {
        let host = MockHost::with_sessions(sample_sessions());
        let runtime_state = runtime_state();
        let ack = kill_all_tabs(&host, &runtime_state, true).expect("kill all should succeed");
        assert!(ack.ok);
        let (kill_count, all_force) = {
            let kills = host.kills.lock().expect("kill log lock should succeed");
            (kills.len(), kills.iter().all(|request| request.force))
        };
        assert_eq!(kill_count, 2);
        assert!(all_force);
    }

    #[test]
    fn kill_tab_passes_selector_and_force_local() {
        let host = MockHost::with_sessions(sample_sessions());
        let target = host
            .sessions
            .first()
            .expect("sample sessions should exist")
            .id;

        let runtime_state = runtime_state();
        let ack = kill_tab(&host, &runtime_state, context_selector_by_id(target), true)
            .expect("kill should succeed");
        assert!(ack.ok);
        let target_text = target.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));

        let (kill_count, first_matches_target, first_force) = {
            let kills = host.kills.lock().expect("kill log lock should succeed");
            (
                kills.len(),
                kills.first().is_some_and(|k| k.selector.id == Some(target)),
                kills.first().is_some_and(|k| k.force),
            )
        };
        assert_eq!(kill_count, 1);
        assert!(first_matches_target);
        assert!(first_force);
    }

    #[test]
    fn switch_tab_requires_target_context_to_exist() {
        let host = MockHost::with_sessions(sample_sessions());
        let last_selected_by_client = LastSelectedByClient::default();
        let runtime_state = runtime_state();
        let error = switch_tab(
            &host,
            &runtime_state,
            context_selector_by_id(Uuid::new_v4()),
            &last_selected_by_client,
            None,
        )
        .expect_err("switch should fail when context is missing");
        assert!(error.contains("not found"));
    }

    #[test]
    fn switch_tab_returns_selected_session_id() {
        let sessions = sample_sessions();
        let target_id = sessions[1].id;
        let host = MockHost::with_sessions(sessions);
        let last_selected_by_client = LastSelectedByClient::default();
        let runtime_state = runtime_state();

        let ack = switch_tab(
            &host,
            &runtime_state,
            context_selector_by_id(target_id),
            &last_selected_by_client,
            None,
        )
        .expect("switch should succeed");
        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));

        let selects: Vec<_> = host
            .selects
            .lock()
            .expect("select log lock should succeed")
            .clone();
        assert_eq!(selects.as_slice(), &[target_id]);
    }

    #[test]
    fn switch_tab_succeeds_when_current_client_query_fails() {
        let host = MockHost::with_client_query_failure();
        let target_id = host
            .sessions
            .get(1)
            .expect("sample sessions should include second item")
            .id;
        let last_selected_by_client = LastSelectedByClient::default();
        let runtime_state = runtime_state();

        let ack = switch_tab(
            &host,
            &runtime_state,
            context_selector_by_id(target_id),
            &last_selected_by_client,
            None,
        )
        .expect("switch should succeed even if current client query fails");
        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));
    }

    #[test]
    fn next_tab_selects_second_session() {
        let sessions = sample_sessions();
        let target_id = sessions[1].id;
        let host = MockHost::with_sessions(sessions.clone());
        seed_tab_order(&host, &sessions);
        let last_selected_by_client = LastSelectedByClient::default();
        let runtime_state = runtime_state();

        let ack = cycle_tab(
            &host,
            &runtime_state,
            TabCycleDirection::Next,
            &last_selected_by_client,
            None,
        )
        .expect("next tab should succeed");
        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));
    }

    #[test]
    fn prev_tab_selects_last_session() {
        let sessions = vec![
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("alpha".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("beta".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("gamma".to_string()),
                attributes: BTreeMap::new(),
            },
        ];
        let target_id = sessions[2].id;
        let host = MockHost::with_sessions(sessions.clone());
        seed_tab_order(&host, &sessions);
        let last_selected_by_client = LastSelectedByClient::default();
        let runtime_state = runtime_state();

        let ack = cycle_tab(
            &host,
            &runtime_state,
            TabCycleDirection::Previous,
            &last_selected_by_client,
            None,
        )
        .expect("previous tab should succeed");
        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));
    }

    #[test]
    fn cycle_tab_follows_stable_order_when_mru_updates() {
        let sessions = sample_sessions_three();
        let first_id = sessions[0].id;
        let second_id = sessions[1].id;
        let third_id = sessions[2].id;
        let host = MockHost::with_sessions(sessions.clone());
        seed_tab_order(&host, &sessions);
        let last_selected_by_client = LastSelectedByClient::default();
        let runtime_state = runtime_state();

        let next = cycle_tab(
            &host,
            &runtime_state,
            TabCycleDirection::Next,
            &last_selected_by_client,
            None,
        )
        .expect("next tab should succeed");
        let second_text = second_id.to_string();
        assert_eq!(next.id.as_deref(), Some(second_text.as_str()));

        let next_again = cycle_tab(
            &host,
            &runtime_state,
            TabCycleDirection::Next,
            &last_selected_by_client,
            None,
        )
        .expect("second next tab should succeed");
        let third_text = third_id.to_string();
        assert_eq!(next_again.id.as_deref(), Some(third_text.as_str()));

        let previous = cycle_tab(
            &host,
            &runtime_state,
            TabCycleDirection::Previous,
            &last_selected_by_client,
            None,
        )
        .expect("previous tab should succeed");
        assert_eq!(previous.id.as_deref(), Some(second_text.as_str()));

        let selects = host
            .selects
            .lock()
            .expect("select log lock should succeed")
            .clone();
        assert_eq!(selects, vec![second_id, third_id, second_id]);

        let stored_order = get_stored_tab_order_ids(&host).expect("order lookup should succeed");
        assert_eq!(stored_order, vec![first_id, second_id, third_id]);
    }

    #[test]
    fn list_tabs_keeps_stable_order_after_switches() {
        let sessions = sample_sessions_three();
        let first_id = sessions[0].id;
        let second_id = sessions[1].id;
        let third_id = sessions[2].id;
        let host = MockHost::with_sessions(sessions.clone());
        seed_tab_order(&host, &sessions);
        let last_selected_by_client = LastSelectedByClient::default();
        let runtime_state = runtime_state();

        let _ = cycle_tab(
            &host,
            &runtime_state,
            TabCycleDirection::Next,
            &last_selected_by_client,
            None,
        )
        .expect("next tab should succeed");

        let tabs = list_tabs(&host, &runtime_state, None).expect("list should succeed");
        assert_eq!(tabs.len(), 3);
        let tab_ids = tabs.iter().map(|tab| tab.id.as_str()).collect::<Vec<_>>();
        let first_text = first_id.to_string();
        let second_text = second_id.to_string();
        let third_text = third_id.to_string();
        assert_eq!(
            tab_ids,
            vec![
                first_text.as_str(),
                second_text.as_str(),
                third_text.as_str()
            ]
        );
        assert!(tabs.iter().any(|tab| tab.active && tab.id == second_text));
    }

    #[test]
    fn empty_tab_order_initializes_to_deterministic_order_not_mru() {
        let first_id = Uuid::from_u128(1);
        let second_id = Uuid::from_u128(2);
        let third_id = Uuid::from_u128(3);
        let sessions = vec![
            SessionSummary {
                id: third_id,
                name: Some("gamma".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: first_id,
                name: Some("alpha".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: second_id,
                name: Some("beta".to_string()),
                attributes: BTreeMap::new(),
            },
        ];
        let host = MockHost::with_sessions(sessions);
        *host
            .mru_context_ids
            .lock()
            .expect("mru context lock should succeed") = vec![third_id, second_id, first_id];
        let runtime_state = runtime_state();

        let tabs = list_tabs(&host, &runtime_state, None).expect("list should succeed");
        let ids = tabs.iter().map(|tab| tab.id.as_str()).collect::<Vec<_>>();
        let first_text = first_id.to_string();
        let second_text = second_id.to_string();
        let third_text = third_id.to_string();
        assert_eq!(
            ids,
            vec![
                first_text.as_str(),
                second_text.as_str(),
                third_text.as_str()
            ]
        );

        let stored_order = get_stored_tab_order_ids_for_workspace(&host, Uuid::nil())
            .expect("workspace order lookup should succeed");
        assert_eq!(stored_order, vec![first_id, second_id, third_id]);

        *host
            .mru_context_ids
            .lock()
            .expect("mru context lock should succeed") = vec![second_id, third_id, first_id];

        let tabs = list_tabs(&host, &runtime_state, None).expect("second list should succeed");
        let ids = tabs.iter().map(|tab| tab.id.as_str()).collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                first_text.as_str(),
                second_text.as_str(),
                third_text.as_str()
            ]
        );
    }

    #[test]
    fn last_tab_requires_alternate_session() {
        let sessions = vec![SessionSummary {
            id: Uuid::new_v4(),
            name: Some("solo".to_string()),
            attributes: BTreeMap::new(),
        }];
        let host = MockHost::with_sessions(sessions);
        let last_selected_by_client = LastSelectedByClient::default();
        let runtime_state = runtime_state();
        let error = cycle_tab(
            &host,
            &runtime_state,
            TabCycleDirection::Last,
            &last_selected_by_client,
            None,
        )
        .expect_err("last tab should require alternate session");
        assert!(error.contains("no alternate tab"));
    }

    #[test]
    fn last_tab_selects_recorded_previous_session() {
        let sessions = sample_sessions();
        let target_id = sessions[0].id;
        let host = MockHost::with_sessions(sessions);
        let last_selected_by_client = LastSelectedByClient::default();
        let runtime_state = runtime_state();

        let _ = cycle_tab(
            &host,
            &runtime_state,
            TabCycleDirection::Next,
            &last_selected_by_client,
            None,
        )
        .expect("next tab should succeed");

        let ack = cycle_tab(
            &host,
            &runtime_state,
            TabCycleDirection::Last,
            &last_selected_by_client,
            None,
        )
        .expect("last tab should use remembered selection");

        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));
    }

    #[test]
    fn create_tab_propagates_host_error() {
        let host = MockHost::with_failures(true, false, false);
        let runtime_state = runtime_state();
        let error = create_tab(&host, &runtime_state, Some("dev".to_string()))
            .expect_err("create should surface host failure");
        assert!(error.contains("mock create failure"), "error was: {error}");
    }

    #[test]
    fn kill_tab_propagates_host_error() {
        let host = MockHost::with_failures(false, true, false);
        let runtime_state = runtime_state();
        let error = kill_tab(&host, &runtime_state, selector_by_name("alpha"), false)
            .expect_err("kill should surface host failure");
        assert!(error.contains("mock kill failure"));
    }

    #[test]
    fn kill_all_tabs_propagates_host_error() {
        let host = MockHost::with_failures(false, true, false);
        let runtime_state = runtime_state();
        let error = kill_all_tabs(&host, &runtime_state, true)
            .expect_err("kill all should fail on host error");
        assert!(error.contains("mock kill failure"));
    }

    #[test]
    fn switch_tab_propagates_context_select_error() {
        let host = MockHost::with_failures(false, true, false);
        let target = host
            .sessions
            .first()
            .expect("sample sessions should exist")
            .id;
        let last_selected_by_client = LastSelectedByClient::default();
        let runtime_state = runtime_state();
        let error = switch_tab(
            &host,
            &runtime_state,
            context_selector_by_id(target),
            &last_selected_by_client,
            None,
        )
        .expect_err("switch should fail when select fails");
        assert!(error.contains("mock select failure"), "error was: {error}");
    }

    #[test]
    fn invoke_service_new_returns_ack_with_id() {
        let _router = install_context_test_router(false, false);
        let plugin = TabsPlugin::default();
        let context = service_test_context(
            "tabs-commands",
            "new-tab",
            encode_service_message(&NewTabArgs {
                name: Some("ok".to_string()),
            })
            .expect("request should encode"),
            "bmux.tabs.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );
        let result: Result<TabAck, TabError> =
            decode_service_message(&response.payload).expect("result should decode");
        let ack = result.expect("new-tab should succeed");
        assert!(ack.ok);
        assert!(ack.id.is_some());
    }

    #[test]
    fn invoke_service_new_surfaces_denied_error() {
        let _router = install_context_test_router(false, false);
        let plugin = TabsPlugin::default();
        let context = service_test_context(
            "tabs-commands",
            "new-tab",
            encode_service_message(&NewTabArgs {
                name: Some("deny".to_string()),
            })
            .expect("request should encode"),
            "bmux.tabs.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        assert!(response.error.is_none());
        let result: Result<TabAck, TabError> =
            decode_service_message(&response.payload).expect("result should decode");
        let error = result.expect_err("expected tab error");
        assert!(
            matches!(error, TabError::Failed { reason } if reason.contains("session policy denied"))
        );
    }

    #[test]
    fn invoke_service_switch_returns_ack_with_selected_id() {
        let _router = install_context_test_router(false, false);
        let plugin = TabsPlugin::default();
        let context = service_test_context(
            "tabs-commands",
            "switch-tab",
            encode_service_message(&SwitchTabArgs {
                target: "alpha".to_string(),
            })
            .expect("request should encode"),
            "bmux.tabs.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );
        let result: Result<TabAck, TabError> =
            decode_service_message(&response.payload).expect("result should decode");
        let ack = result.expect("switch-tab should succeed");
        assert!(ack.ok);
        assert!(ack.id.is_some_and(|id| !id.is_empty()));
    }

    fn installed_switch_request(
        resources: &bmux_plugin::AttachPresentationResources,
        target: Uuid,
    ) -> Vec<u8> {
        let endpoint = bmux_plugin::AttachInputEndpoint {
            capability: "bmux.tab_bar.input".into(),
            interface_id: "presentation-input".into(),
            operation: "handle-input".into(),
        };
        let mut event = bmux_plugin::AttachInputEvent {
            hook_id: format!("bmux.tab_bar:strip:tab:{target}"),
            event_kind: "pointer".into(),
            phase: "down".into(),
            button: Some("right".into()),
            key: None,
            col: None,
            row: None,
            wheel_delta: 0,
            modifiers: bmux_plugin::AttachInputModifiers::default(),
            focused_pane: None,
            hovered_pane: None,
        };
        assert!(resources.input.invoke(&endpoint, &event).unwrap().consumed);
        event.event_kind = "key".into();
        event.phase = "press".into();
        event.key = Some("enter".into());
        let result = resources.input.invoke(&endpoint, &event).unwrap();
        assert!(result.consumed && result.release_capture);
        let invocation = result.service_invocation.unwrap();
        assert_eq!(invocation.endpoint.operation, "switch-tab");
        invocation.payload
    }

    fn installed_strip_ops(
        resources: &bmux_plugin::AttachPresentationResources,
    ) -> Vec<bmux_plugin::RenderOp> {
        resources
            .surfaces
            .surfaces()
            .into_iter()
            .flat_map(|surface| surface.ops)
            .collect()
    }

    #[allow(clippy::result_large_err)] // TestServiceRouter requires the SDK's unboxed PluginError.
    fn real_context_router() -> (
        bmux_plugin::test_support::TestServiceRouterGuard,
        Arc<std::sync::RwLock<bmux_contexts_plugin::ContextState>>,
        [Uuid; 2],
    ) {
        let client = bmux_session_models::ClientId(Uuid::from_u128(99));
        let mut state = bmux_contexts_plugin::ContextState::default();
        let alpha = state
            .create(client, Some("alpha".into()), BTreeMap::new())
            .id;
        let beta = state
            .create(client, Some("beta".into()), BTreeMap::new())
            .id;
        let state = Arc::new(std::sync::RwLock::new(state));
        bmux_plugin::global_plugin_state_registry().register(&state);
        let router = bmux_plugin::test_support::install_test_service_router(Arc::new(
            move |_, _, capability, kind, interface, operation, payload| {
                if interface.starts_with("contexts-") {
                    let mut context =
                        service_test_context(interface, operation, payload, capability, kind);
                    context.plugin_id = "bmux.contexts".into();
                    context.caller_client_id = Some(client.0);
                    let response = bmux_contexts_plugin::ContextsPlugin.invoke_service(context);
                    assert!(response.error.is_none(), "{:?}", response.error);
                    Ok(response.payload)
                } else if interface == "storage-query/v1" {
                    encode_service_message(&bmux_plugin_sdk::StorageGetResponse { value: None })
                } else if interface == "volatile-state-query/v1" {
                    encode_service_message(&bmux_plugin_sdk::VolatileStateGetResponse {
                        value: None,
                    })
                } else if matches!(
                    interface,
                    "storage-command/v1" | "volatile-state-command/v1"
                ) {
                    encode_service_message(&())
                } else {
                    Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                        operation: "real_context_router",
                    })
                }
            },
        ));
        (router, state, [alpha, beta])
    }

    fn assert_selected_context(
        state: &std::sync::RwLock<bmux_contexts_plugin::ContextState>,
        expected: Uuid,
    ) {
        assert_eq!(
            state
                .read()
                .unwrap()
                .selected_by_client
                .get(&bmux_session_models::ClientId(Uuid::from_u128(99))),
            Some(&expected)
        );
    }

    async fn dispatch_tab_over_transport(payload: Vec<u8>) -> Vec<u8> {
        use bmux_ipc::transport::ErasedIpcStream;
        use bmux_ipc::{Envelope, EnvelopeKind, Request, Response, ResponsePayload};
        let (client_stream, server_stream) = tokio::io::duplex(8192);
        let server = async move {
            let mut stream = ErasedIpcStream::new(Box::new(server_stream));
            for step in 0..2 {
                let envelope = stream.recv_envelope().await.unwrap();
                let request: Request = bmux_ipc::decode(&envelope.payload).unwrap();
                let response = if step == 0 {
                    let Request::Hello { contract, .. } = request else {
                        panic!("expected hello")
                    };
                    ResponsePayload::HelloNegotiated {
                        negotiated: bmux_ipc::NegotiatedProtocol {
                            wire_epoch: contract.wire_epoch,
                            revision: contract.revisions.max,
                            capabilities: Vec::new(),
                        },
                    }
                } else {
                    let Request::InvokeService {
                        capability,
                        interface_id,
                        operation,
                        payload,
                        ..
                    } = request
                    else {
                        panic!("expected invocation")
                    };
                    let response = TabsPlugin::default().invoke_service(service_test_context(
                        &interface_id,
                        &operation,
                        payload,
                        &capability,
                        ServiceKind::Command,
                    ));
                    assert!(response.error.is_none(), "{:?}", response.error);
                    ResponsePayload::ServiceInvoked {
                        payload: response.payload,
                    }
                };
                stream
                    .send_envelope(&Envelope::new(
                        envelope.request_id,
                        EnvelopeKind::Response,
                        bmux_ipc::encode(&Response::Ok(response)).unwrap(),
                    ))
                    .await
                    .unwrap();
            }
        };
        let client = async move {
            let client = bmux_client::BmuxClient::connect_with_bridge_stream(
                ErasedIpcStream::new(Box::new(client_stream)),
                std::time::Duration::from_secs(2),
                "tab-integration",
                Uuid::from_u128(99),
            )
            .await
            .unwrap();
            let mut client = bmux_client::StreamingBmuxClient::from_client(client).unwrap();
            client
                .invoke_service_raw(
                    "bmux.tabs.write",
                    bmux_ipc::InvokeServiceKind::Command,
                    "tabs-commands",
                    "switch-tab",
                    payload,
                )
                .await
                .unwrap()
        };
        let ((), response) = tokio::join!(server, client);
        response
    }

    #[test]
    fn service_switch_publication_reaches_installed_strip() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (_router, context_state, [alpha, beta]) = real_context_router();
            let events = bmux_plugin::global_event_bus();
            events.register_state_channel(
                bmux_tabs_plugin_api::tabs_list::STATE_KIND,
                bmux_tabs_plugin_api::tabs_list::TabListSnapshot {
                    tabs: Vec::new(),
                    revision: 0,
                },
            );
            events.register_state_channel(
                bmux_plugin_sdk::PluginEventKind::from_static("bmux.attach/local-presentation"),
                bmux_attach_view_protocol::AttachLocalPresentationSnapshot {
                    viewport_cols: 80,
                    viewport_rows: 24,
                    ..bmux_attach_view_protocol::AttachLocalPresentationSnapshot::initial()
                },
            );
            events.register_state_channel(
                bmux_tabs_plugin_api::tabs_local_view::STATE_KIND,
                bmux_tabs_plugin_api::tabs_local_view::TabSelection { context_id: None },
            );
            let resources = bmux_plugin::AttachPresentationResources {
                events: events.clone(),
                layouts: Arc::new(bmux_plugin::layout::PluginLayoutRegistry::new(64)),
                allocations: Arc::new(bmux_plugin::layout::AllocationRegistry::default()),
                surfaces: Arc::new(bmux_plugin::surface::PluginSurfaceRegistry::new(64)),
                input: Arc::new(bmux_plugin::AttachPresentationInputRegistry::new()),
            };
            let settings: toml::Value = toml::from_str("preset = 'classic'\ntab_template = '{name}'").unwrap();
            let _installation =
                bmux_tab_bar_plugin::TabBarPresentation::install(Some(&settings), &resources).unwrap();
            for (target, expected_id) in [("alpha", alpha), ("beta", beta)] {
                let response = dispatch_tab_over_transport(
                    if expected_id == alpha {
                        encode_service_message(&SwitchTabArgs { target: target.into() }).unwrap()
                    } else {
                        installed_switch_request(&resources, expected_id)
                    },
                ).await;
                let result: Result<TabAck, TabError> = decode_service_message(&response).unwrap();
                assert_eq!(result.unwrap().id, Some(expected_id.to_string()));
                let (catalog, _) = events
                    .subscribe_state::<bmux_tabs_plugin_api::tabs_list::TabListSnapshot>(
                        &bmux_tabs_plugin_api::tabs_list::STATE_KIND,
                    )
                    .unwrap();
                assert_eq!(
                    catalog
                        .tabs
                        .iter()
                        .find(|entry| entry.active)
                        .unwrap()
                        .id,
                    expected_id
                );
                assert_selected_context(&context_state, expected_id);
                let expected = format!("({target})");
                for _ in 0..20 {
                    tokio::task::yield_now().await;
                    if installed_strip_ops(&resources).iter().any(|op| matches!(op, bmux_plugin::RenderOp::TextRun { text, .. } if text == &expected)) {
                        break;
                    }
                }
                let ops = installed_strip_ops(&resources);
                assert!(ops.iter().any(|op| matches!(op, bmux_plugin::RenderOp::TextRun { text, .. } if text == &expected)), "missing selected tab {expected}: {ops:?}");
                let inactive = if target == "alpha" { "(beta)" } else { "(alpha)" };
                assert!(!ops.iter().any(|op| matches!(op, bmux_plugin::RenderOp::TextRun { text, .. } if text == inactive)));
            }
        });
    }

    #[test]
    fn selected_context_ack_records_command_outcome_metadata() {
        let context_id = Uuid::from_u128(42);
        bmux_plugin_sdk::begin_command_outcome_capture();
        record_selected_context_outcome(&TabAck {
            ok: true,
            id: Some(context_id.to_string()),
        });

        let outcome = bmux_plugin_sdk::finish_command_outcome_capture();
        let expected = context_id.to_string();
        assert_eq!(
            outcome
                .metadata
                .get(COMMAND_OUTCOME_SELECTED_CONTEXT_ID_KEY)
                .and_then(serde_json::Value::as_str),
            Some(expected.as_str())
        );
    }

    #[test]
    fn invoke_service_rejects_invalid_payload() {
        let plugin = TabsPlugin::default();
        let context = service_test_context(
            "tabs-commands",
            "kill-tab",
            vec![1, 2, 3],
            "bmux.tabs.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected service error");
        assert_eq!(error.code, "invalid_request");
    }

    #[test]
    fn invoke_service_move_floating_pane_is_wired() {
        let plugin = TabsPlugin::default();
        let context = service_test_context(
            "tabs-commands",
            "move-floating-pane",
            vec![1, 2, 3],
            "bmux.tabs.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected invalid request");
        assert_eq!(error.code, "invalid_request");
    }

    /// `restart-pane` must reach the pane-runtime primitive rather than
    /// short-circuiting with the old `unsupported` stub. The test router
    /// does not serve `clients-state`, so session resolution fails and
    /// the error surfaces under the command's own `restart_failed` code —
    /// proof the request was dispatched instead of rejected up front.
    #[test]
    fn invoke_service_restart_pane_is_wired() {
        let _router = install_context_test_router(false, false);
        let plugin = TabsPlugin::default();
        let context = service_test_context(
            "tabs-commands",
            "restart-pane",
            encode_service_message(&RestartPaneArgs {
                session: None,
                target: None,
            })
            .expect("request should encode"),
            "bmux.tabs.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected restart dispatch failure");
        assert_eq!(
            error.code, "restart_failed",
            "restart-pane must dispatch, not return the `unsupported` stub",
        );
    }

    #[test]
    fn invoke_service_kill_surfaces_denied_error() {
        let _router = install_context_test_router(false, true);
        let plugin = TabsPlugin::default();
        let context = service_test_context(
            "tabs-commands",
            "kill-tab",
            encode_service_message(&KillTabArgs {
                target: "deny".to_string(),
                force_local: false,
            })
            .expect("request should encode"),
            "bmux.tabs.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        assert!(response.error.is_none());
        let result: Result<TabAck, TabError> =
            decode_service_message(&response.payload).expect("result should decode");
        let error = result.expect_err("expected tab error");
        assert!(
            matches!(error, TabError::Failed { reason } if reason.contains("session policy denied"))
        );
    }

    #[test]
    fn invoke_service_rejects_unsupported_operation() {
        let plugin = TabsPlugin::default();
        let context = service_test_context(
            "tabs-commands",
            "unknown",
            Vec::new(),
            "bmux.tabs.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        let error = response
            .error
            .expect("expected unsupported operation error");
        assert_eq!(error.code, "unsupported_service_operation");
    }

    #[test]
    fn workspace_filter_keeps_only_matching_contexts() {
        let default_context = ContextSummary {
            id: Uuid::from_u128(1),
            name: Some("default".to_string()),
            attributes: BTreeMap::from([("workspace".to_string(), "default".to_string())]),
        };
        let workspace_id = Uuid::from_u128(2);
        let workspace_context = ContextSummary {
            id: Uuid::from_u128(3),
            name: Some("project".to_string()),
            attributes: BTreeMap::from([("workspace".to_string(), workspace_id.to_string())]),
        };

        let filtered = filter_contexts_for_workspace(
            vec![default_context.clone(), workspace_context.clone()],
            workspace_id,
        );
        assert_eq!(filtered, vec![workspace_context]);
        assert_eq!(
            filter_contexts_for_workspace(vec![default_context.clone()], Uuid::nil()),
            vec![default_context]
        );
    }

    #[test]
    fn goto_tab_by_index_selects_first_context() {
        let sessions = sample_sessions();
        let first_id = sessions[0].id;
        let host = MockHost::with_sessions(sessions.clone());
        seed_tab_order(&host, &sessions);
        let last_selected_by_client = LastSelectedByClient::default();
        let runtime_state = runtime_state();

        let ack = goto_tab_by_index(&host, &runtime_state, 1, &last_selected_by_client, None)
            .expect("goto index 1 should succeed");
        assert!(ack.ok);
        let first_text = first_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(first_text.as_str()));
    }

    #[test]
    fn goto_tab_by_index_selects_second_context() {
        let sessions = sample_sessions();
        let second_id = sessions[1].id;
        let host = MockHost::with_sessions(sessions.clone());
        seed_tab_order(&host, &sessions);
        let last_selected_by_client = LastSelectedByClient::default();
        let runtime_state = runtime_state();

        let ack = goto_tab_by_index(&host, &runtime_state, 2, &last_selected_by_client, None)
            .expect("goto index 2 should succeed");
        assert!(ack.ok);
        let second_text = second_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(second_text.as_str()));
    }

    #[test]
    fn goto_tab_by_index_rejects_zero() {
        let host = MockHost::with_sessions(sample_sessions());
        let last_selected_by_client = LastSelectedByClient::default();
        let runtime_state = runtime_state();

        let error = goto_tab_by_index(&host, &runtime_state, 0, &last_selected_by_client, None)
            .expect_err("index 0 should fail");
        assert!(error.contains("1 or greater"));
    }

    #[test]
    fn goto_tab_by_index_rejects_out_of_range() {
        let host = MockHost::with_sessions(sample_sessions());
        let last_selected_by_client = LastSelectedByClient::default();
        let runtime_state = runtime_state();

        let error = goto_tab_by_index(&host, &runtime_state, 99, &last_selected_by_client, None)
            .expect_err("index 99 should fail");
        assert!(error.contains("out of range"));
    }

    #[test]
    fn last_tab_closed_setting_defaults_to_delete_and_accepts_keep_empty() {
        assert_eq!(on_last_tab_closed_setting(None).unwrap(), "delete");
        let keep_empty = toml::toml! { on_last_tab_closed = "keep_empty" }.into();
        assert_eq!(
            on_last_tab_closed_setting(Some(&keep_empty)).unwrap(),
            "keep_empty"
        );
        let invalid = toml::toml! { on_last_tab_closed = "invalid" }.into();
        assert!(on_last_tab_closed_setting(Some(&invalid)).is_err());
    }

    #[test]
    fn close_current_tab_closes_and_switches() {
        let sessions = sample_sessions();
        let first_id = sessions[0].id;
        let host = MockHost::with_sessions(sessions);
        let last_selected_by_client = LastSelectedByClient::default();
        let runtime_state = runtime_state();

        let ack = close_current_tab(&host, &runtime_state, &last_selected_by_client, None, None)
            .expect("close current should succeed");
        assert!(ack.ok);
        let first_text = first_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(first_text.as_str()));

        // Verify that a context select was issued (switch to fallback tab)
        let has_selects = !host
            .selects
            .lock()
            .expect("select log lock should succeed")
            .is_empty();
        assert!(has_selects, "should have switched to a fallback tab");

        // Verify that the current tab was closed
        let (kill_count, first_kill_matches) = {
            let kills = host.kills.lock().expect("kill log lock should succeed");
            (
                kills.len(),
                kills
                    .first()
                    .is_some_and(|k| k.selector.id == Some(first_id)),
            )
        };
        assert_eq!(kill_count, 1);
        assert!(first_kill_matches);
    }

    #[test]
    fn move_tab_moves_source_before_target_and_persists_order() {
        let sessions = sample_sessions_three();
        let first = sessions[0].id;
        let second = sessions[1].id;
        let third = sessions[2].id;
        let host = MockHost::with_sessions(sessions.clone());
        let runtime_state = runtime_state();
        seed_tab_order(&host, &sessions);

        let ack = move_tab(
            &host,
            &runtime_state,
            third,
            first,
            TabMovePlacement::Before,
        )
        .expect("move should succeed");

        assert!(ack.ok);
        assert_eq!(ack.id, Some(third.to_string()));
        let order =
            get_stored_tab_order_ids_for_workspace(&host, Uuid::nil()).expect("order readable");
        assert_eq!(order, vec![third, first, second]);
        assert_eq!(cached_tab_order_ids(&runtime_state), Some(order));
    }

    #[test]
    fn move_tab_moves_source_after_target_and_keeps_active_unchanged() {
        let sessions = sample_sessions_three();
        let first = sessions[0].id;
        let second = sessions[1].id;
        let third = sessions[2].id;
        let host = MockHost::with_sessions(sessions.clone());
        let runtime_state = runtime_state();
        seed_tab_order(&host, &sessions);
        set_stored_context_id(&host, ACTIVE_TAB_CONTEXT_KEY, Some(second)).expect("seed active");

        move_tab(&host, &runtime_state, first, third, TabMovePlacement::After)
            .expect("move should succeed");

        let order =
            get_stored_tab_order_ids_for_workspace(&host, Uuid::nil()).expect("order readable");
        assert_eq!(order, vec![second, third, first]);
        assert_eq!(
            get_stored_context_id(&host, ACTIVE_TAB_CONTEXT_KEY).expect("active readable"),
            Some(second)
        );
    }

    #[test]
    fn move_tab_same_source_and_target_is_noop() {
        let sessions = sample_sessions_three();
        let first = sessions[0].id;
        let host = MockHost::with_sessions(sessions.clone());
        let runtime_state = runtime_state();
        seed_tab_order(&host, &sessions);

        move_tab(&host, &runtime_state, first, first, TabMovePlacement::After)
            .expect("same source and target should succeed");

        let order = get_stored_tab_order_ids(&host).expect("order readable");
        assert_eq!(
            order,
            sessions
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn move_tab_rejects_unknown_source_or_target() {
        let sessions = sample_sessions();
        let first = sessions[0].id;
        let host = MockHost::with_sessions(sessions.clone());
        let runtime_state = runtime_state();
        seed_tab_order(&host, &sessions);
        let unknown = Uuid::new_v4();

        let source_error = move_tab(
            &host,
            &runtime_state,
            unknown,
            first,
            TabMovePlacement::Before,
        )
        .expect_err("unknown source should fail");
        assert!(source_error.contains("source tab context not found"));

        let target_error = move_tab(
            &host,
            &runtime_state,
            first,
            unknown,
            TabMovePlacement::Before,
        )
        .expect_err("unknown target should fail");
        assert!(target_error.contains("target tab context not found"));
    }

    /// Verify that `register_typed_services` installs both typed
    /// handles (`tabs-state` Query, `tabs-commands` Command) in
    /// the registry and that they downcast to the generated BPDL
    /// service trait objects.
    #[test]
    fn register_typed_services_installs_both_typed_handles() {
        let plugin = TabsPlugin::default();
        let mut registry = TypedServiceRegistry::new();
        let empty_caps: Vec<String> = Vec::new();
        let services: Vec<bmux_plugin_sdk::RegisteredService> = Vec::new();
        let settings = std::collections::BTreeMap::new();
        let host_metadata = bmux_plugin_sdk::HostMetadata {
            product_name: "test".to_string(),
            product_version: "0".to_string(),
            plugin_api_version: bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION,
            plugin_abi_version: bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION,
        };
        let host_connection = bmux_plugin_sdk::HostConnectionInfo {
            config_dir: "/tmp".to_string(),
            config_dir_candidates: vec!["/tmp".to_string()],
            runtime_dir: "/tmp".to_string(),
            data_dir: "/tmp".to_string(),
            state_dir: "/tmp".to_string(),
        };
        let context = TypedServiceRegistrationContext {
            plugin_id: "bmux.tabs",
            host_kernel_bridge: None,
            required_capabilities: &empty_caps,
            provided_capabilities: &empty_caps,
            services: &services,
            available_capabilities: &empty_caps,
            enabled_plugins: &empty_caps,
            plugin_search_roots: &empty_caps,
            host: &host_metadata,
            connection: &host_connection,
            plugin_settings_map: &settings,
        };
        plugin.register_typed_services(context, &mut registry);

        let read_cap = HostScope::new("bmux.tabs.read").expect("read capability");
        let write_cap = HostScope::new("bmux.tabs.write").expect("write capability");

        let state_handle = registry
            .get(
                &read_cap,
                ServiceKind::Query,
                tabs_state::INTERFACE_ID.as_str(),
            )
            .expect("state handle registered");
        let _state = state_handle
            .provider_as_trait::<dyn TabsStateService + Send + Sync>()
            .expect("state handle downcasts to typed trait");

        let commands_handle = registry
            .get(
                &write_cap,
                ServiceKind::Command,
                tabs_commands::INTERFACE_ID.as_str(),
            )
            .expect("commands handle registered");
        let _commands = commands_handle
            .provider_as_trait::<dyn TabsCommandsService + Send + Sync>()
            .expect("commands handle downcasts to typed trait");
    }

    /// Simulates three `ContextEvent::Created` events arriving in
    /// sequence on the contexts-events channel — exactly the stream
    /// the real subscriber receives. The expected post-state is that
    /// `tabs.order` contains A, B, C in that exact order.
    #[test]
    fn append_context_to_tab_order_preserves_arrival_sequence() {
        let host = MockHost::with_sessions(Vec::new());
        let a = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
        let b = Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222);
        let c = Uuid::from_u128(0x3333_3333_3333_3333_3333_3333_3333_3333);

        let runtime_state = runtime_state();
        append_context_to_tab_order(&host, &runtime_state, a).expect("append A");
        append_context_to_tab_order(&host, &runtime_state, b).expect("append B");
        append_context_to_tab_order(&host, &runtime_state, c).expect("append C");

        let order = get_stored_tab_order_ids(&host).expect("order readable");
        assert_eq!(order, vec![a, b, c]);
    }

    /// Duplicate `Created` events for the same id must not push
    /// duplicates into `tabs.order`.
    #[test]
    fn append_context_to_tab_order_is_idempotent() {
        let host = MockHost::with_sessions(Vec::new());
        let a = Uuid::from_u128(0xAAAA_AAAA_AAAA_AAAA_AAAA_AAAA_AAAA_AAAA);

        let runtime_state = runtime_state();
        append_context_to_tab_order(&host, &runtime_state, a).expect("first append");
        append_context_to_tab_order(&host, &runtime_state, a).expect("second append");

        let order = get_stored_tab_order_ids(&host).expect("order readable");
        assert_eq!(order, vec![a]);
    }

    #[test]
    fn legacy_context_without_workspace_attribute_belongs_to_default_workspace() {
        let context = ContextSummary {
            id: Uuid::from_u128(1),
            name: Some("legacy".to_string()),
            attributes: BTreeMap::new(),
        };

        assert_eq!(context_workspace_id(&context), Uuid::nil());
        assert_eq!(
            filter_contexts_for_workspace(vec![context.clone()], Uuid::nil()),
            vec![context]
        );
    }

    /// Simulates a `ContextEvent::Closed` for a middle entry. The
    /// remaining entries preserve their relative order.
    #[test]
    fn legacy_flat_order_migrates_to_default_workspace_without_reordering() {
        let host = MockHost::with_sessions(Vec::new());
        let legacy = [
            Uuid::from_u128(30),
            Uuid::from_u128(10),
            Uuid::from_u128(20),
        ];
        set_stored_tab_order_ids(&host, &legacy).expect("legacy order should seed");

        let migrated = get_stored_tab_order_ids_for_workspace(&host, Uuid::nil())
            .expect("default workspace order should migrate");
        assert_eq!(migrated, legacy);

        set_stored_tab_order_ids(&host, &[]).expect("legacy order should clear");
        let persisted = get_stored_tab_order_ids_for_workspace(&host, Uuid::nil())
            .expect("migrated order should remain");
        assert_eq!(persisted, legacy);
    }

    #[test]
    fn workspace_order_keys_are_isolated_by_uuid() {
        let host = MockHost::with_sessions(Vec::new());
        let first_workspace = Uuid::from_u128(1);
        let second_workspace = Uuid::from_u128(2);
        let first_order = [Uuid::from_u128(10), Uuid::from_u128(11)];
        let second_order = [Uuid::from_u128(20)];
        set_stored_tab_order_ids_for_workspace(&host, first_workspace, &first_order)
            .expect("first workspace order should persist");
        set_stored_tab_order_ids_for_workspace(&host, second_workspace, &second_order)
            .expect("second workspace order should persist");

        assert_eq!(
            get_stored_tab_order_ids_for_workspace(&host, first_workspace).unwrap(),
            first_order
        );
        assert_eq!(
            get_stored_tab_order_ids_for_workspace(&host, second_workspace).unwrap(),
            second_order
        );
    }

    #[test]
    fn remove_context_from_tab_order_preserves_surrounding_order() {
        let host = MockHost::with_sessions(Vec::new());
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let c = Uuid::from_u128(3);
        let runtime_state = runtime_state();

        set_stored_tab_order_ids(&host, &[a, b, c]).expect("seed order");
        remove_context_from_tab_order(&host, &runtime_state, b).expect("remove B");

        let order = get_stored_tab_order_ids(&host).expect("order readable");
        assert_eq!(order, vec![a, c]);
    }

    /// Closing the currently active context also clears the
    /// `ACTIVE_TAB_CONTEXT_KEY` marker so stale pointers don't
    /// linger.
    #[test]
    fn remove_context_from_tab_order_clears_stale_active_marker() {
        let host = MockHost::with_sessions(Vec::new());
        let a = Uuid::from_u128(42);
        let runtime_state = runtime_state();

        set_stored_tab_order_ids(&host, &[a]).expect("seed order");
        set_stored_context_id(&host, ACTIVE_TAB_CONTEXT_KEY, Some(a)).expect("set active");
        remove_context_from_tab_order(&host, &runtime_state, a).expect("remove A");

        let active = get_stored_context_id(&host, ACTIVE_TAB_CONTEXT_KEY).expect("active readable");
        assert!(active.is_none());
    }

    /// `Selected` event promotes the target into
    /// `ACTIVE_TAB_CONTEXT_KEY` and demotes the previous active
    /// into `PREVIOUS_TAB_CONTEXT_KEY` so `last-tab` still works.
    #[test]
    fn mark_context_active_promotes_previous_to_last_tab_slot() {
        let host = MockHost::with_sessions(Vec::new());
        let a = Uuid::from_u128(11);
        let b = Uuid::from_u128(22);
        let runtime_state = runtime_state();

        set_stored_context_id(&host, ACTIVE_TAB_CONTEXT_KEY, Some(a)).expect("seed active = A");
        mark_context_active(&host, &runtime_state, b).expect("mark B active");

        assert_eq!(
            get_stored_context_id(&host, ACTIVE_TAB_CONTEXT_KEY).expect("active readable"),
            Some(b)
        );
        assert_eq!(
            get_runtime_context_id(&host, &runtime_state, PREVIOUS_TAB_CONTEXT_KEY)
                .expect("previous readable"),
            Some(a)
        );
    }

    /// Re-selecting the already-active context is a no-op on the
    /// previous-tab slot (no spurious swap to itself).
    #[test]
    fn mark_context_active_is_idempotent_when_already_active() {
        let host = MockHost::with_sessions(Vec::new());
        let a = Uuid::from_u128(7);
        let runtime_state = runtime_state();

        set_stored_context_id(&host, ACTIVE_TAB_CONTEXT_KEY, Some(a)).expect("seed active");
        mark_context_active(&host, &runtime_state, a).expect("re-mark active");

        assert_eq!(
            get_stored_context_id(&host, ACTIVE_TAB_CONTEXT_KEY).expect("active readable"),
            Some(a)
        );
        assert_eq!(
            get_runtime_context_id(&host, &runtime_state, PREVIOUS_TAB_CONTEXT_KEY)
                .expect("previous readable"),
            None
        );
    }

    #[test]
    fn runtime_state_is_plugin_instance_scoped() {
        let host = MockHost::with_sessions(Vec::new());
        let first_runtime_state = runtime_state();
        let second_runtime_state = runtime_state();
        let active = Uuid::from_u128(99);

        set_runtime_context_id(
            &host,
            &first_runtime_state,
            ACTIVE_TAB_CONTEXT_KEY,
            Some(active),
        )
        .expect("first runtime state should be writable");

        assert_eq!(
            in_memory_runtime_context_id(&first_runtime_state, ACTIVE_TAB_CONTEXT_KEY),
            Some(active)
        );
        assert_eq!(
            in_memory_runtime_context_id(&second_runtime_state, ACTIVE_TAB_CONTEXT_KEY),
            None
        );
    }
}
