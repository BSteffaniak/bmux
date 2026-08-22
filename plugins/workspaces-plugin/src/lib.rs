//! Named workspace metadata and switching.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_clients_plugin_api::clients_state;
use bmux_contexts_plugin_api::{contexts_commands, contexts_state};
use bmux_plugin::{
    HostRuntimeApi, ServiceCaller, TypedServiceCaller, global_event_bus,
    global_plugin_state_registry,
};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    StorageGetRequest, StorageSetRequest, TypedServiceRegistrationContext, TypedServiceRegistry,
};
use bmux_workspaces_plugin_api::workspaces_commands::{
    self, WorkspaceAck, WorkspaceCommandError, WorkspacesCommandsService,
};
use bmux_workspaces_plugin_api::workspaces_events::{self, WorkspaceEvent};
use bmux_workspaces_plugin_api::workspaces_state::{
    self, WorkspaceQueryError, WorkspaceSelector, WorkspaceSummary, WorkspacesStateService,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

const DEFAULT_WORKSPACE_NAME: &str = "default";
const DEFAULT_WORKSPACE_ATTRIBUTE: &str = "default";
const SELECTED_CONTEXT_OUTCOME_KEY: &str = "bmux.contexts.selected_context_id";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WorkspaceRecord {
    id: Uuid,
    name: String,
}

#[derive(Debug, Default)]
struct WorkspaceState {
    records: Vec<WorkspaceRecord>,
    active_by_client: HashMap<Uuid, Uuid>,
    previous_by_client: HashMap<Uuid, Uuid>,
}

impl WorkspaceState {
    fn ensure_default(&mut self) {
        if self.records.is_empty() {
            self.records.push(WorkspaceRecord {
                id: Uuid::nil(),
                name: DEFAULT_WORKSPACE_NAME.to_string(),
            });
        }
    }

    fn resolve(&self, selector: &WorkspaceSelector) -> Option<&WorkspaceRecord> {
        selector
            .id
            .and_then(|id| self.records.iter().find(|workspace| workspace.id == id))
            .or_else(|| {
                selector
                    .name
                    .as_deref()
                    .and_then(|name| self.records.iter().find(|workspace| workspace.name == name))
            })
    }

    fn active_id(&self, client_id: Uuid) -> Uuid {
        self.active_by_client
            .get(&client_id)
            .copied()
            .filter(|id| self.records.iter().any(|workspace| workspace.id == *id))
            .unwrap_or_else(Uuid::nil)
    }

    fn select(&mut self, client_id: Uuid, workspace_id: Uuid) {
        let current = self.active_id(client_id);
        if current != workspace_id {
            self.previous_by_client.insert(client_id, current);
            self.active_by_client.insert(client_id, workspace_id);
        }
    }
}

#[derive(Default)]
pub struct WorkspacesPlugin;

impl RustPlugin for WorkspacesPlugin {
    type Contract = bmux_workspaces_plugin_api::Contract;

    fn activate(&mut self, context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        let records = load_catalog(&context).unwrap_or_default();
        let mut state = WorkspaceState {
            records,
            ..WorkspaceState::default()
        };
        state.ensure_default();
        let state = Arc::new(RwLock::new(state));
        global_plugin_state_registry().register::<WorkspaceState>(&state);
        global_event_bus().register_channel::<WorkspaceEvent>(workspaces_events::EVENT_KIND);
        Ok(EXIT_OK)
    }

    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        run_command(&context).map_err(PluginCommandError::failed)?;
        Ok(EXIT_OK)
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "workspaces-state", "list-workspaces" => |_req: (), ctx| {
                list_workspaces(ctx).map_err(|error| ServiceResponse::error("list_failed", error))
            },
            "workspaces-state", "get-workspace" => |req: SelectorArgs, ctx| {
                Ok::<Result<WorkspaceSummary, WorkspaceQueryError>, ServiceResponse>(
                    get_workspace(ctx, &req.selector)
                )
            },
            "workspaces-state", "current-workspace" => |_req: (), ctx| {
                current_workspace(ctx).map_err(|error| ServiceResponse::error("current_failed", error))
            },
            "workspaces-commands", "new-workspace" => |req: NewWorkspaceArgs, ctx| {
                Ok::<Result<WorkspaceAck, WorkspaceCommandError>, ServiceResponse>(
                    new_workspace(ctx, req.name)
                )
            },
            "workspaces-commands", "rename-workspace" => |req: RenameWorkspaceArgs, ctx| {
                Ok::<Result<WorkspaceAck, WorkspaceCommandError>, ServiceResponse>(
                    rename_workspace(ctx, &req.selector, req.name)
                )
            },
            "workspaces-commands", "kill-workspace" => |req: SelectorArgs, ctx| {
                Ok::<Result<WorkspaceAck, WorkspaceCommandError>, ServiceResponse>(
                    kill_workspace(ctx, &req.selector)
                )
            },
            "workspaces-commands", "switch-workspace" => |req: SelectorArgs, ctx| {
                Ok::<Result<WorkspaceAck, WorkspaceCommandError>, ServiceResponse>(
                    switch_workspace(ctx, &req.selector)
                )
            },
            "workspaces-commands", "next-workspace" => |_req: (), ctx| {
                Ok::<Result<WorkspaceAck, WorkspaceCommandError>, ServiceResponse>(
                    cycle_workspace(ctx, 1)
                )
            },
            "workspaces-commands", "prev-workspace" => |_req: (), ctx| {
                Ok::<Result<WorkspaceAck, WorkspaceCommandError>, ServiceResponse>(
                    cycle_workspace(ctx, -1)
                )
            },
            "workspaces-commands", "last-workspace" => |_req: (), ctx| {
                Ok::<Result<WorkspaceAck, WorkspaceCommandError>, ServiceResponse>(
                    last_workspace(ctx)
                )
            },
            "workspaces-commands", "move-tab-to-workspace" => |req: MoveTabArgs, ctx| {
                Ok::<Result<WorkspaceAck, WorkspaceCommandError>, ServiceResponse>(
                    move_tab(ctx, req.context_id, &req.workspace)
                )
            },
        })
    }

    fn register_typed_services(
        &self,
        context: TypedServiceRegistrationContext<'_>,
        registry: &mut TypedServiceRegistry,
    ) {
        let caller = Arc::new(TypedServiceCaller::from_registration_context(&context));
        let state: Arc<dyn WorkspacesStateService + Send + Sync> =
            Arc::new(WorkspacesStateHandle {
                caller: Arc::clone(&caller),
            });
        let commands: Arc<dyn WorkspacesCommandsService + Send + Sync> =
            Arc::new(WorkspacesCommandsHandle { caller });
        let _ = workspaces_state::register_provider(registry, state);
        let _ = workspaces_commands::register_provider(registry, commands);
    }
}

#[derive(Serialize, Deserialize)]
struct SelectorArgs {
    selector: WorkspaceSelector,
}

#[derive(Serialize, Deserialize)]
struct NewWorkspaceArgs {
    name: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct RenameWorkspaceArgs {
    selector: WorkspaceSelector,
    name: String,
}

#[derive(Serialize, Deserialize)]
struct MoveTabArgs {
    context_id: Uuid,
    workspace: WorkspaceSelector,
}

fn state() -> Result<Arc<RwLock<WorkspaceState>>, String> {
    global_plugin_state_registry()
        .get::<WorkspaceState>()
        .ok_or_else(|| "workspace state is not active".to_string())
}

const fn dispatch_client<C: ServiceCaller + Sync + ?Sized>(
    caller: &C,
) -> bmux_plugin::ServiceCallerDispatchClient<'_, C> {
    bmux_plugin::ServiceCallerDispatchClient::new(caller)
}

fn client_id(caller_client_id: Option<Uuid>) -> Result<Uuid, WorkspaceCommandError> {
    caller_client_id.ok_or_else(|| WorkspaceCommandError::Failed {
        reason: "workspace operation requires a caller client id".to_string(),
    })
}

fn validate_name(name: Option<String>, ordinal: usize) -> Result<String, WorkspaceCommandError> {
    let name = name.unwrap_or_else(|| format!("workspace-{ordinal}"));
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(WorkspaceCommandError::InvalidName {
            reason: "name must not be empty".to_string(),
        });
    }
    Ok(trimmed.to_string())
}

fn workspace_attribute(id: Uuid) -> String {
    if id.is_nil() {
        DEFAULT_WORKSPACE_ATTRIBUTE.to_string()
    } else {
        id.to_string()
    }
}

fn context_workspace_id(context: &contexts_state::ContextSummary) -> Uuid {
    context
        .attributes
        .get("workspace")
        .filter(|value| value.as_str() != DEFAULT_WORKSPACE_ATTRIBUTE)
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap_or_else(Uuid::nil)
}

fn list_contexts(
    caller: &(impl ServiceCaller + Sync),
) -> Result<Vec<contexts_state::ContextSummary>, String> {
    let mut client = dispatch_client(caller);
    bmux_plugin::block_on_typed_dispatch(contexts_state::client::list_contexts(&mut client))
        .map_err(|error| format!("contexts-state/list-contexts failed: {error}"))
}

fn select_context(
    caller: &(impl ServiceCaller + Sync),
    context_id: Uuid,
) -> Result<(), WorkspaceCommandError> {
    let mut client = dispatch_client(caller);
    let result = bmux_plugin::block_on_typed_dispatch(contexts_commands::client::select_context(
        &mut client,
        contexts_state::ContextSelector {
            id: Some(context_id),
            name: None,
        },
    ))
    .map_err(|error| WorkspaceCommandError::Failed {
        reason: format!("contexts-commands/select-context failed: {error}"),
    })?;
    result
        .map(|_| ())
        .map_err(|error| WorkspaceCommandError::Failed {
            reason: format!("select-context failed: {error:?}"),
        })
}

fn set_context_workspace(
    caller: &(impl ServiceCaller + Sync),
    context: &contexts_state::ContextSummary,
    workspace_id: Uuid,
) -> Result<(), WorkspaceCommandError> {
    let mut attributes = context.attributes.clone();
    attributes.insert("workspace".to_string(), workspace_attribute(workspace_id));
    let mut client = dispatch_client(caller);
    let result =
        bmux_plugin::block_on_typed_dispatch(contexts_commands::client::set_context_attributes(
            &mut client,
            contexts_state::ContextSelector {
                id: Some(context.id),
                name: None,
            },
            attributes,
        ))
        .map_err(|error| WorkspaceCommandError::Failed {
            reason: format!("set-context-attributes failed: {error}"),
        })?;
    result
        .map(|_| ())
        .map_err(|error| WorkspaceCommandError::Failed {
            reason: format!("set-context-attributes failed: {error:?}"),
        })
}

fn list_workspaces(caller: &(impl ServiceCaller + Sync)) -> Result<Vec<WorkspaceSummary>, String> {
    let contexts = list_contexts(caller)?;
    let state = state()?;
    let guard = state
        .read()
        .map_err(|_| "workspace state lock poisoned".to_string())?;
    let active = resolve_client_id(caller).map_or_else(Uuid::nil, |id| guard.active_id(id));
    Ok(guard
        .records
        .iter()
        .map(|workspace| WorkspaceSummary {
            id: workspace.id,
            name: workspace.name.clone(),
            tab_ids: contexts
                .iter()
                .filter(|context| context_workspace_id(context) == workspace.id)
                .map(|context| context.id)
                .collect(),
            active: workspace.id == active,
        })
        .collect())
}

fn resolve_client_id(caller: &(impl ServiceCaller + Sync)) -> Option<Uuid> {
    let mut client = dispatch_client(caller);
    bmux_plugin::block_on_typed_dispatch(clients_state::client::current_client(&mut client))
        .ok()
        .and_then(Result::ok)
        .map(|summary| summary.id)
}

fn list_workspaces_for_client(
    caller: &(impl ServiceCaller + Sync),
    caller_client_id: Option<Uuid>,
) -> Result<Vec<WorkspaceSummary>, String> {
    let contexts = list_contexts(caller)?;
    let state = state()?;
    let guard = state
        .read()
        .map_err(|_| "workspace state lock poisoned".to_string())?;
    let active = caller_client_id.map_or_else(Uuid::nil, |id| guard.active_id(id));
    Ok(guard
        .records
        .iter()
        .map(|workspace| WorkspaceSummary {
            id: workspace.id,
            name: workspace.name.clone(),
            tab_ids: contexts
                .iter()
                .filter(|context| context_workspace_id(context) == workspace.id)
                .map(|context| context.id)
                .collect(),
            active: workspace.id == active,
        })
        .collect())
}

fn get_workspace(
    caller: &(impl ServiceCaller + Sync),
    selector: &WorkspaceSelector,
) -> Result<WorkspaceSummary, WorkspaceQueryError> {
    list_workspaces(caller)
        .map_err(|reason| WorkspaceQueryError::InvalidSelector { reason })?
        .into_iter()
        .find(|workspace| {
            selector.id == Some(workspace.id)
                || selector.name.as_deref() == Some(workspace.name.as_str())
        })
        .ok_or(WorkspaceQueryError::NotFound)
}

fn current_workspace(
    caller: &(impl ServiceCaller + Sync),
) -> Result<Option<WorkspaceSummary>, String> {
    Ok(list_workspaces(caller)?
        .into_iter()
        .find(|workspace| workspace.active))
}

fn new_workspace(
    caller: &(impl HostRuntimeApi + Sync),
    name: Option<String>,
) -> Result<WorkspaceAck, WorkspaceCommandError> {
    let client_id = client_id(resolve_client_id(caller))?;
    let state = state().map_err(|reason| WorkspaceCommandError::Failed { reason })?;
    let (record, records) = {
        let mut guard = state.write().map_err(|_| WorkspaceCommandError::Failed {
            reason: "workspace state lock poisoned".to_string(),
        })?;
        let name = validate_name(name, guard.records.len().saturating_add(1))?;
        let record = WorkspaceRecord {
            id: Uuid::new_v4(),
            name,
        };
        guard.records.push(record.clone());
        guard.select(client_id, record.id);
        (record, guard.records.clone())
    };
    save_catalog(caller, &records)?;
    let _ = global_event_bus().emit(
        &workspaces_events::EVENT_KIND,
        WorkspaceEvent::Created {
            workspace_id: record.id,
            name: record.name,
        },
    );
    Ok(WorkspaceAck {
        id: record.id,
        selected_context_id: None,
    })
}

fn rename_workspace(
    caller: &(impl HostRuntimeApi + Sync),
    selector: &WorkspaceSelector,
    name: String,
) -> Result<WorkspaceAck, WorkspaceCommandError> {
    let state = state().map_err(|reason| WorkspaceCommandError::Failed { reason })?;
    let (id, name, records) = {
        let mut guard = state.write().map_err(|_| WorkspaceCommandError::Failed {
            reason: "workspace state lock poisoned".to_string(),
        })?;
        let id = guard
            .resolve(selector)
            .map(|record| record.id)
            .ok_or(WorkspaceCommandError::NotFound)?;
        let name = validate_name(Some(name), guard.records.len())?;
        let record = guard
            .records
            .iter_mut()
            .find(|record| record.id == id)
            .ok_or(WorkspaceCommandError::NotFound)?;
        record.name.clone_from(&name);
        (id, name, guard.records.clone())
    };
    save_catalog(caller, &records)?;
    let _ = global_event_bus().emit(
        &workspaces_events::EVENT_KIND,
        WorkspaceEvent::Renamed {
            workspace_id: id,
            name,
        },
    );
    Ok(WorkspaceAck {
        id,
        selected_context_id: None,
    })
}

fn kill_workspace(
    caller: &(impl HostRuntimeApi + Sync),
    selector: &WorkspaceSelector,
) -> Result<WorkspaceAck, WorkspaceCommandError> {
    let contexts =
        list_contexts(caller).map_err(|reason| WorkspaceCommandError::Failed { reason })?;
    let state = state().map_err(|reason| WorkspaceCommandError::Failed { reason })?;
    let (id, fallback_id, records) = {
        let mut guard = state.write().map_err(|_| WorkspaceCommandError::Failed {
            reason: "workspace state lock poisoned".to_string(),
        })?;
        if guard.records.len() == 1 {
            return Err(WorkspaceCommandError::CannotRemoveLastWorkspace);
        }
        let id = guard
            .resolve(selector)
            .map(|record| record.id)
            .ok_or(WorkspaceCommandError::NotFound)?;
        let fallback_id = guard
            .records
            .iter()
            .find(|record| record.id != id)
            .map(|record| record.id)
            .ok_or(WorkspaceCommandError::CannotRemoveLastWorkspace)?;
        guard.records.retain(|record| record.id != id);
        for active in guard.active_by_client.values_mut() {
            if *active == id {
                *active = fallback_id;
            }
        }
        for previous in guard.previous_by_client.values_mut() {
            if *previous == id {
                *previous = fallback_id;
            }
        }
        (id, fallback_id, guard.records.clone())
    };
    for context in contexts
        .iter()
        .filter(|context| context_workspace_id(context) == id)
    {
        set_context_workspace(caller, context, fallback_id)?;
    }
    save_catalog(caller, &records)?;
    let _ = global_event_bus().emit(
        &workspaces_events::EVENT_KIND,
        WorkspaceEvent::Removed { workspace_id: id },
    );
    Ok(WorkspaceAck {
        id,
        selected_context_id: None,
    })
}

fn switch_workspace(
    caller: &(impl HostRuntimeApi + Sync),
    selector: &WorkspaceSelector,
) -> Result<WorkspaceAck, WorkspaceCommandError> {
    let client_id = client_id(resolve_client_id(caller))?;
    switch_workspace_for_client(caller, selector, client_id)
}

fn switch_workspace_for_client(
    caller: &(impl HostRuntimeApi + Sync),
    selector: &WorkspaceSelector,
    client_id: Uuid,
) -> Result<WorkspaceAck, WorkspaceCommandError> {
    let contexts =
        list_contexts(caller).map_err(|reason| WorkspaceCommandError::Failed { reason })?;
    let state = state().map_err(|reason| WorkspaceCommandError::Failed { reason })?;
    let workspace_id = {
        let guard = state.read().map_err(|_| WorkspaceCommandError::Failed {
            reason: "workspace state lock poisoned".to_string(),
        })?;
        guard
            .resolve(selector)
            .map(|record| record.id)
            .ok_or(WorkspaceCommandError::NotFound)?
    };
    let context_id = contexts
        .iter()
        .find(|context| context_workspace_id(context) == workspace_id)
        .map(|context| context.id);
    if let Some(context_id) = context_id {
        select_context(caller, context_id)?;
    }
    {
        let mut guard = state.write().map_err(|_| WorkspaceCommandError::Failed {
            reason: "workspace state lock poisoned".to_string(),
        })?;
        guard.select(client_id, workspace_id);
    }
    if let Some(context_id) = context_id {
        let _ = global_event_bus().emit(
            &workspaces_events::EVENT_KIND,
            WorkspaceEvent::Selected {
                workspace_id,
                context_id,
                initiator_client_id: Some(client_id),
            },
        );
    }
    Ok(WorkspaceAck {
        id: workspace_id,
        selected_context_id: context_id,
    })
}

fn cycle_workspace(
    caller: &(impl HostRuntimeApi + Sync),
    offset: isize,
) -> Result<WorkspaceAck, WorkspaceCommandError> {
    let client_id = client_id(resolve_client_id(caller))?;
    let state = state().map_err(|reason| WorkspaceCommandError::Failed { reason })?;
    let target = {
        let guard = state.read().map_err(|_| WorkspaceCommandError::Failed {
            reason: "workspace state lock poisoned".to_string(),
        })?;
        let current = guard.active_id(client_id);
        let index = guard
            .records
            .iter()
            .position(|record| record.id == current)
            .unwrap_or(0);
        let len = guard.records.len();
        let target = if offset < 0 {
            index
                .checked_sub(1)
                .unwrap_or_else(|| len.saturating_sub(1))
        } else {
            index.saturating_add(1) % len
        };
        guard.records[target].id
    };
    switch_workspace_for_client(
        caller,
        &WorkspaceSelector {
            id: Some(target),
            name: None,
        },
        client_id,
    )
}

fn last_workspace(
    caller: &(impl HostRuntimeApi + Sync),
) -> Result<WorkspaceAck, WorkspaceCommandError> {
    let client_id = client_id(resolve_client_id(caller))?;
    let state = state().map_err(|reason| WorkspaceCommandError::Failed { reason })?;
    let previous = state
        .read()
        .map_err(|_| WorkspaceCommandError::Failed {
            reason: "workspace state lock poisoned".to_string(),
        })?
        .previous_by_client
        .get(&client_id)
        .copied()
        .ok_or(WorkspaceCommandError::NoPreviousWorkspace)?;
    switch_workspace_for_client(
        caller,
        &WorkspaceSelector {
            id: Some(previous),
            name: None,
        },
        client_id,
    )
}

fn move_tab(
    caller: &(impl HostRuntimeApi + Sync),
    context_id: Uuid,
    selector: &WorkspaceSelector,
) -> Result<WorkspaceAck, WorkspaceCommandError> {
    let workspace_id = {
        let state = state().map_err(|reason| WorkspaceCommandError::Failed { reason })?;
        let guard = state.read().map_err(|_| WorkspaceCommandError::Failed {
            reason: "workspace state lock poisoned".to_string(),
        })?;
        guard
            .resolve(selector)
            .map(|record| record.id)
            .ok_or(WorkspaceCommandError::NotFound)?
    };
    let context = list_contexts(caller)
        .map_err(|reason| WorkspaceCommandError::Failed { reason })?
        .into_iter()
        .find(|context| context.id == context_id)
        .ok_or(WorkspaceCommandError::NotFound)?;
    set_context_workspace(caller, &context, workspace_id)?;
    Ok(WorkspaceAck {
        id: workspace_id,
        selected_context_id: Some(context_id),
    })
}

fn load_catalog(caller: &impl HostRuntimeApi) -> Result<Vec<WorkspaceRecord>, String> {
    let response = caller
        .storage_get(&StorageGetRequest::new(bmux_plugin_sdk::storage_key!(
            "workspaces.catalog"
        )))
        .map_err(|error| error.to_string())?;
    response.value.map_or_else(
        || Ok(Vec::new()),
        |value| serde_json::from_slice(&value).map_err(|error| error.to_string()),
    )
}

fn save_catalog(
    caller: &impl HostRuntimeApi,
    records: &[WorkspaceRecord],
) -> Result<(), WorkspaceCommandError> {
    let value = serde_json::to_vec(records).map_err(|error| WorkspaceCommandError::Failed {
        reason: error.to_string(),
    })?;
    caller
        .storage_set(&StorageSetRequest::new(
            bmux_plugin_sdk::storage_key!("workspaces.catalog"),
            value,
        ))
        .map_err(|error| WorkspaceCommandError::Failed {
            reason: error.to_string(),
        })
}

fn option_value(arguments: &[String], option: &str) -> Option<String> {
    let long = format!("--{option}");
    arguments.windows(2).find_map(|pair| {
        (pair.first().map(String::as_str) == Some(long.as_str())).then(|| pair[1].clone())
    })
}

fn positional_value_at(arguments: &[String], target_index: usize) -> Option<String> {
    let mut positional_index = 0usize;
    let mut index = 0usize;
    while index < arguments.len() {
        if arguments[index].starts_with('-') {
            index = index.saturating_add(2);
            continue;
        }
        if positional_index == target_index {
            return Some(arguments[index].clone());
        }
        positional_index = positional_index.saturating_add(1);
        index = index.saturating_add(1);
    }
    None
}

fn run_command(context: &NativeCommandContext) -> Result<(), String> {
    match context.command.as_str() {
        "list-workspaces" => {
            for workspace in list_workspaces_for_client(context, context.caller_client_id)? {
                println!("{}\t{}", workspace.id, workspace.name);
            }
            Ok(())
        }
        "new-workspace" => {
            let ack = new_workspace_for_context(context, option_value(&context.arguments, "name"))?;
            record_outcome(&ack);
            Ok(())
        }
        "switch-workspace" => command_switch(context, &parse_selector_argument(context, 0)?, false),
        "next-workspace" => command_cycle(context, 1),
        "prev-workspace" => command_cycle(context, -1),
        "last-workspace" => command_last(context),
        "rename-workspace" => {
            let selector = parse_selector_argument(context, 0)?;
            let name = positional_value_at(&context.arguments, 1)
                .ok_or_else(|| "missing NAME".to_string())?;
            rename_workspace(context, &selector, name).map_err(|error| format!("{error:?}"))?;
            Ok(())
        }
        "kill-workspace" => {
            let selector = parse_selector_argument(context, 0)?;
            kill_workspace(context, &selector).map_err(|error| format!("{error:?}"))?;
            Ok(())
        }
        "move-tab-to-workspace" => {
            let context_id = positional_value_at(&context.arguments, 0)
                .ok_or_else(|| "missing CONTEXT_ID".to_string())
                .and_then(|value| Uuid::parse_str(&value).map_err(|error| error.to_string()))?;
            let selector = parse_selector_argument(context, 1)?;
            move_tab(context, context_id, &selector).map_err(|error| format!("{error:?}"))?;
            Ok(())
        }
        command => Err(format!("unsupported command '{command}'")),
    }
}

fn new_workspace_for_context(
    context: &NativeCommandContext,
    name: Option<String>,
) -> Result<WorkspaceAck, String> {
    let client_id = context
        .caller_client_id
        .ok_or_else(|| "workspace operation requires caller client id".to_string())?;
    let state = state()?;
    let (record, records) = {
        let mut guard = state
            .write()
            .map_err(|_| "workspace state lock poisoned".to_string())?;
        let name = validate_name(name, guard.records.len().saturating_add(1))
            .map_err(|error| format!("{error:?}"))?;
        let record = WorkspaceRecord {
            id: Uuid::new_v4(),
            name,
        };
        guard.records.push(record.clone());
        guard.select(client_id, record.id);
        (record, guard.records.clone())
    };
    save_catalog(context, &records).map_err(|error| format!("{error:?}"))?;
    let _ = global_event_bus().emit(
        &workspaces_events::EVENT_KIND,
        WorkspaceEvent::Created {
            workspace_id: record.id,
            name: record.name,
        },
    );
    Ok(WorkspaceAck {
        id: record.id,
        selected_context_id: None,
    })
}

fn command_switch(
    context: &NativeCommandContext,
    selector: &WorkspaceSelector,
    _unused: bool,
) -> Result<(), String> {
    let client_id = context
        .caller_client_id
        .ok_or_else(|| "workspace operation requires caller client id".to_string())?;
    let ack = switch_workspace_for_client(context, selector, client_id)
        .map_err(|error| format!("{error:?}"))?;
    record_outcome(&ack);
    Ok(())
}

fn command_cycle(context: &NativeCommandContext, offset: isize) -> Result<(), String> {
    let client_id = context
        .caller_client_id
        .ok_or_else(|| "workspace operation requires caller client id".to_string())?;
    let target = {
        let state = state()?;
        let guard = state
            .read()
            .map_err(|_| "workspace state lock poisoned".to_string())?;
        let current = guard.active_id(client_id);
        let index = guard
            .records
            .iter()
            .position(|record| record.id == current)
            .unwrap_or(0);
        let len = guard.records.len();
        if offset < 0 {
            guard.records[index
                .checked_sub(1)
                .unwrap_or_else(|| len.saturating_sub(1))]
            .id
        } else {
            guard.records[index.saturating_add(1) % len].id
        }
    };
    command_switch(
        context,
        &WorkspaceSelector {
            id: Some(target),
            name: None,
        },
        false,
    )
}

fn command_last(context: &NativeCommandContext) -> Result<(), String> {
    let client_id = context
        .caller_client_id
        .ok_or_else(|| "workspace operation requires caller client id".to_string())?;
    let previous = state()?
        .read()
        .map_err(|_| "workspace state lock poisoned".to_string())?
        .previous_by_client
        .get(&client_id)
        .copied()
        .ok_or_else(|| "no previous workspace".to_string())?;
    command_switch(
        context,
        &WorkspaceSelector {
            id: Some(previous),
            name: None,
        },
        false,
    )
}

fn parse_selector_argument(
    context: &NativeCommandContext,
    index: usize,
) -> Result<WorkspaceSelector, String> {
    let target = positional_value_at(&context.arguments, index)
        .ok_or_else(|| "missing TARGET".to_string())?;
    Ok(Uuid::parse_str(&target).map_or_else(
        |_| WorkspaceSelector {
            id: None,
            name: Some(target),
        },
        |id| WorkspaceSelector {
            id: Some(id),
            name: None,
        },
    ))
}

fn record_outcome(ack: &WorkspaceAck) {
    if let Some(context_id) = ack.selected_context_id {
        bmux_plugin_sdk::record_command_outcome_metadata(
            SELECTED_CONTEXT_OUTCOME_KEY,
            serde_json::json!(context_id),
        );
    }
}

struct WorkspacesStateHandle {
    caller: Arc<TypedServiceCaller>,
}
impl WorkspacesStateService for WorkspacesStateHandle {
    fn list_workspaces<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Vec<WorkspaceSummary>> + Send + 'a>> {
        Box::pin(async move { list_workspaces(self.caller.as_ref()).unwrap_or_default() })
    }
    fn get_workspace<'a>(
        &'a self,
        selector: WorkspaceSelector,
    ) -> Pin<Box<dyn Future<Output = Result<WorkspaceSummary, WorkspaceQueryError>> + Send + 'a>>
    {
        Box::pin(async move { get_workspace(self.caller.as_ref(), &selector) })
    }
    fn current_workspace<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Option<WorkspaceSummary>> + Send + 'a>> {
        Box::pin(async move { current_workspace(self.caller.as_ref()).unwrap_or(None) })
    }
}

struct WorkspacesCommandsHandle {
    caller: Arc<TypedServiceCaller>,
}
impl WorkspacesCommandsService for WorkspacesCommandsHandle {
    fn new_workspace<'a>(
        &'a self,
        name: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<WorkspaceAck, WorkspaceCommandError>> + Send + 'a>>
    {
        Box::pin(async move { new_workspace(self.caller.as_ref(), name) })
    }
    fn rename_workspace<'a>(
        &'a self,
        selector: WorkspaceSelector,
        name: String,
    ) -> Pin<Box<dyn Future<Output = Result<WorkspaceAck, WorkspaceCommandError>> + Send + 'a>>
    {
        Box::pin(async move { rename_workspace(self.caller.as_ref(), &selector, name) })
    }
    fn kill_workspace<'a>(
        &'a self,
        selector: WorkspaceSelector,
    ) -> Pin<Box<dyn Future<Output = Result<WorkspaceAck, WorkspaceCommandError>> + Send + 'a>>
    {
        Box::pin(async move { kill_workspace(self.caller.as_ref(), &selector) })
    }
    fn switch_workspace<'a>(
        &'a self,
        selector: WorkspaceSelector,
    ) -> Pin<Box<dyn Future<Output = Result<WorkspaceAck, WorkspaceCommandError>> + Send + 'a>>
    {
        Box::pin(async move { switch_workspace(self.caller.as_ref(), &selector) })
    }
    fn next_workspace<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<WorkspaceAck, WorkspaceCommandError>> + Send + 'a>>
    {
        Box::pin(async move { cycle_workspace(self.caller.as_ref(), 1) })
    }
    fn prev_workspace<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<WorkspaceAck, WorkspaceCommandError>> + Send + 'a>>
    {
        Box::pin(async move { cycle_workspace(self.caller.as_ref(), -1) })
    }
    fn last_workspace<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<WorkspaceAck, WorkspaceCommandError>> + Send + 'a>>
    {
        Box::pin(async move { last_workspace(self.caller.as_ref()) })
    }
    fn move_tab_to_workspace<'a>(
        &'a self,
        context_id: Uuid,
        workspace: WorkspaceSelector,
    ) -> Pin<Box<dyn Future<Output = Result<WorkspaceAck, WorkspaceCommandError>> + Send + 'a>>
    {
        Box::pin(async move { move_tab(self.caller.as_ref(), context_id, &workspace) })
    }
}

bmux_plugin_sdk::export_plugin!(WorkspacesPlugin, include_str!("../plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workspace_is_stable_and_selected() {
        let client_id = Uuid::from_u128(1);
        let mut state = WorkspaceState::default();
        state.ensure_default();

        assert_eq!(state.records.len(), 1);
        assert_eq!(state.records[0].id, Uuid::nil());
        assert_eq!(state.active_id(client_id), Uuid::nil());
    }

    #[test]
    fn selecting_workspace_tracks_previous_per_client() {
        let first_client = Uuid::from_u128(1);
        let second_client = Uuid::from_u128(2);
        let workspace_id = Uuid::from_u128(3);
        let mut state = WorkspaceState::default();
        state.ensure_default();
        state.records.push(WorkspaceRecord {
            id: workspace_id,
            name: "second".to_string(),
        });

        state.select(first_client, workspace_id);

        assert_eq!(state.active_id(first_client), workspace_id);
        assert_eq!(
            state.previous_by_client.get(&first_client),
            Some(&Uuid::nil())
        );
        assert_eq!(state.active_id(second_client), Uuid::nil());
        assert!(!state.previous_by_client.contains_key(&second_client));
    }

    #[test]
    fn workspace_attribute_maps_default_and_uuid_values() {
        assert_eq!(workspace_attribute(Uuid::nil()), "default");
        let id = Uuid::from_u128(42);
        assert_eq!(workspace_attribute(id), id.to_string());
    }
}
