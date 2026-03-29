#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin::HostRuntimeApi;
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    ContextCloseRequest, ContextCreateRequest, ContextSelector, StorageGetRequest,
    StorageSetRequest,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

const ACTIVE_WINDOW_CONTEXT_KEY: &str = "windows.active_context_id";
const PREVIOUS_WINDOW_CONTEXT_KEY: &str = "windows.previous_context_id";

#[derive(Default)]
pub struct WindowsPlugin {
    last_selected_by_client: BTreeMap<Uuid, Uuid>,
}

impl RustPlugin for WindowsPlugin {
    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        handle_command(self, &context)?;
        Ok(EXIT_OK)
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "window-query/v1", "list" => |req: ListWindowsRequest, ctx| {
                let windows = list_windows(ctx, req.session.as_deref())
                    .map_err(|e| ServiceResponse::error("list_failed", e))?;
                Ok(ListWindowsResponse { windows })
            },
            "window-command/v1", "new" => |req: NewWindowRequest, ctx| {
                create_window(ctx, req.name)
                    .map_err(|e| ServiceResponse::error("new_failed", e))
            },
            "window-command/v1", "kill" => |req: KillWindowRequest, ctx| {
                let selector = parse_selector(&req.target)
                    .map_err(|e| ServiceResponse::error("invalid_request", e))?;
                kill_window(ctx, selector, req.force_local)
                    .map_err(|e| ServiceResponse::error("kill_failed", e))
            },
            "window-command/v1", "kill_all" => |req: KillAllWindowsRequest, ctx| {
                kill_all_windows(ctx, req.force_local)
                    .map_err(|e| ServiceResponse::error("kill_failed", e))
            },
            "window-command/v1", "switch" => |req: SwitchWindowRequest, ctx| {
                let selector = parse_selector(&req.target)
                    .map_err(|e| ServiceResponse::error("invalid_request", e))?;
                switch_window(ctx, selector, &mut self.last_selected_by_client)
                    .map_err(|e| ServiceResponse::error("switch_failed", e))
            },
        })
    }
}

fn handle_command(
    plugin: &mut WindowsPlugin,
    context: &NativeCommandContext,
) -> Result<(), String> {
    match context.command.as_str() {
        "new-window" => {
            let name = option_value(&context.arguments, "name");
            let response = context
                .context_create(&ContextCreateRequest {
                    name,
                    attributes: BTreeMap::new(),
                })
                .map_err(|error| error.to_string())?;
            println!("created window context: {}", response.context.id);
            Ok(())
        }
        "list-windows" => {
            let session_filter = option_value(&context.arguments, "session");
            let as_json = has_flag(&context.arguments, "json");
            let windows = list_windows(context, session_filter.as_deref())?;
            if as_json {
                let output = serde_json::to_string_pretty(&ListWindowsResponse { windows })
                    .map_err(|error| error.to_string())?;
                println!("{output}");
            } else if windows.is_empty() {
                println!("no windows");
            } else {
                for window in windows {
                    println!(
                        "{}\t{}\t{}",
                        window.id,
                        window.name,
                        if window.active { "active" } else { "inactive" }
                    );
                }
            }
            Ok(())
        }
        "kill-window" => {
            let target = positional_value(&context.arguments)
                .ok_or_else(|| "missing required TARGET argument".to_string())?;
            let selector = parse_selector(&target)?;
            let force_local = has_flag(&context.arguments, "force-local");
            let response = context
                .context_close(&ContextCloseRequest {
                    selector,
                    force: force_local,
                })
                .map_err(|error| error.to_string())?;
            println!("killed window context: {}", response.id);
            Ok(())
        }
        "kill-all-windows" => {
            let force_local = has_flag(&context.arguments, "force-local");
            let contexts = context
                .context_list()
                .map_err(|error| error.to_string())?
                .contexts;
            if contexts.is_empty() {
                println!("no windows");
                return Ok(());
            }
            for context_summary in contexts {
                let response = context
                    .context_close(&ContextCloseRequest {
                        selector: ContextSelector::ById(context_summary.id),
                        force: force_local,
                    })
                    .map_err(|error| error.to_string())?;
                println!("killed window context: {}", response.id);
            }
            Ok(())
        }
        "switch-window" => {
            let target = positional_value(&context.arguments)
                .ok_or_else(|| "missing required TARGET argument".to_string())?;
            let selector = parse_selector(&target)?;
            let ack = switch_window(context, selector, &mut plugin.last_selected_by_client)?;
            let context_id = ack
                .id
                .ok_or_else(|| "switch-window did not return selected context id".to_string())?;
            println!("active window context: {context_id}");
            Ok(())
        }
        "next-window" => {
            let ack = cycle_window(
                context,
                WindowCycleDirection::Next,
                &mut plugin.last_selected_by_client,
            )?;
            if let Some(id) = ack.id {
                println!("next-window selected context {id}");
            }
            Ok(())
        }
        "prev-window" => {
            let ack = cycle_window(
                context,
                WindowCycleDirection::Previous,
                &mut plugin.last_selected_by_client,
            )?;
            if let Some(id) = ack.id {
                println!("prev-window selected context {id}");
            }
            Ok(())
        }
        "last-window" => {
            let ack = cycle_window(
                context,
                WindowCycleDirection::Last,
                &mut plugin.last_selected_by_client,
            )?;
            if let Some(id) = ack.id {
                println!("last-window selected context {id}");
            }
            Ok(())
        }
        _ => Err(format!("unsupported command '{}'", context.command)),
    }
}

enum WindowCycleDirection {
    Next,
    Previous,
    Last,
}

fn list_windows(
    caller: &impl HostRuntimeApi,
    session_filter: Option<&str>,
) -> Result<Vec<WindowEntry>, String> {
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    let selected = if let Some(filter) = session_filter {
        let selector = parse_selector(filter)?;
        contexts
            .into_iter()
            .filter(|context| match &selector {
                ContextSelector::ById(id) => &context.id == id,
                ContextSelector::ByName(name) => context.name.as_deref() == Some(name.as_str()),
            })
            .collect::<Vec<_>>()
    } else {
        contexts
    };

    Ok(selected
        .into_iter()
        .enumerate()
        .map(|(index, context)| WindowEntry {
            id: context.id.to_string(),
            name: context
                .name
                .unwrap_or_else(|| format!("context-{}", index.saturating_add(1))),
            active: index == 0,
        })
        .collect())
}

fn create_window(
    caller: &impl HostRuntimeApi,
    name: Option<String>,
) -> Result<WindowCommandAck, String> {
    let previous_context = resolve_effective_current_context(caller).ok().flatten();
    let response = caller
        .context_create(&ContextCreateRequest {
            name,
            attributes: BTreeMap::new(),
        })
        .map_err(|error| error.to_string())?;
    let context_id = response.context.id;
    if let Some(previous) = previous_context
        && previous != context_id
    {
        let _ = set_stored_context_id(caller, PREVIOUS_WINDOW_CONTEXT_KEY, Some(previous));
    }
    let _ = set_stored_context_id(caller, ACTIVE_WINDOW_CONTEXT_KEY, Some(context_id));
    Ok(WindowCommandAck {
        ok: true,
        id: Some(context_id.to_string()),
    })
}

fn kill_window(
    caller: &impl HostRuntimeApi,
    selector: ContextSelector,
    force_local: bool,
) -> Result<WindowCommandAck, String> {
    let response = caller
        .context_close(&ContextCloseRequest {
            selector,
            force: force_local,
        })
        .map_err(|error| error.to_string())?;
    Ok(WindowCommandAck {
        ok: true,
        id: Some(response.id.to_string()),
    })
}

fn kill_all_windows(
    caller: &impl HostRuntimeApi,
    force_local: bool,
) -> Result<WindowCommandAck, String> {
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    for context in contexts {
        caller
            .context_close(&ContextCloseRequest {
                selector: ContextSelector::ById(context.id),
                force: force_local,
            })
            .map_err(|error| error.to_string())?;
    }
    Ok(WindowCommandAck { ok: true, id: None })
}

fn switch_window(
    caller: &impl HostRuntimeApi,
    selector: ContextSelector,
    last_selected_by_client: &mut BTreeMap<Uuid, Uuid>,
) -> Result<WindowCommandAck, String> {
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    let previous_context = resolve_effective_current_context_with_contexts(caller, &contexts)?;
    let context_id = resolve_context_id_from_contexts(&contexts, &selector)?;
    caller
        .context_select(&bmux_plugin_sdk::ContextSelectRequest {
            selector: ContextSelector::ById(context_id),
        })
        .map_err(|error| error.to_string())?;
    if let Ok(client) = caller.current_client()
        && let Some(previous) = previous_context
        && previous != context_id
    {
        last_selected_by_client.insert(client.id, previous);
    }
    if let Some(previous) = previous_context
        && previous != context_id
    {
        let _ = set_stored_context_id(caller, PREVIOUS_WINDOW_CONTEXT_KEY, Some(previous));
    }
    let _ = set_stored_context_id(caller, ACTIVE_WINDOW_CONTEXT_KEY, Some(context_id));
    Ok(WindowCommandAck {
        ok: true,
        id: Some(context_id.to_string()),
    })
}

fn cycle_window(
    caller: &impl HostRuntimeApi,
    direction: WindowCycleDirection,
    last_selected_by_client: &mut BTreeMap<Uuid, Uuid>,
) -> Result<WindowCommandAck, String> {
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    if contexts.len() < 2 {
        return Err("no alternate window available".to_string());
    }
    let current_context = resolve_effective_current_context_with_contexts(caller, &contexts)?
        .unwrap_or(contexts[0].id);
    let current_index = contexts
        .iter()
        .position(|context| context.id == current_context)
        .unwrap_or(0);
    let target_id = match direction {
        WindowCycleDirection::Next => contexts[(current_index + 1) % contexts.len()].id,
        WindowCycleDirection::Previous => {
            contexts[(current_index + contexts.len() - 1) % contexts.len()].id
        }
        WindowCycleDirection::Last => {
            let remembered_by_client = caller
                .current_client()
                .ok()
                .and_then(|client| last_selected_by_client.get(&client.id).copied());
            let remembered = remembered_by_client
                .or_else(|| {
                    get_stored_context_id(caller, PREVIOUS_WINDOW_CONTEXT_KEY)
                        .ok()
                        .flatten()
                })
                .ok_or_else(|| "no previously active window available".to_string())?;
            if !contexts.iter().any(|context| context.id == remembered) {
                return Err("no previously active window available".to_string());
            }
            if remembered == current_context {
                return Err("no previously active window available".to_string());
            }
            remembered
        }
    };
    switch_window(
        caller,
        ContextSelector::ById(target_id),
        last_selected_by_client,
    )
}

fn resolve_context_id_from_contexts(
    contexts: &[bmux_plugin_sdk::ContextSummary],
    selector: &ContextSelector,
) -> Result<Uuid, String> {
    contexts
        .iter()
        .find(|context| match selector {
            ContextSelector::ById(id) => context.id == *id,
            ContextSelector::ByName(name) => context.name.as_deref() == Some(name.as_str()),
        })
        .map(|context| context.id)
        .ok_or_else(|| "target context not found".to_string())
}

fn resolve_effective_current_context(caller: &impl HostRuntimeApi) -> Result<Option<Uuid>, String> {
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    resolve_effective_current_context_with_contexts(caller, &contexts)
}

fn resolve_effective_current_context_with_contexts(
    caller: &impl HostRuntimeApi,
    contexts: &[bmux_plugin_sdk::ContextSummary],
) -> Result<Option<Uuid>, String> {
    let current = caller
        .context_current()
        .map_err(|error| error.to_string())?
        .context
        .map(|context| context.id)
        .filter(|id| contexts.iter().any(|context| context.id == *id));
    if current.is_some() {
        return Ok(current);
    }
    let stored_active = get_stored_context_id(caller, ACTIVE_WINDOW_CONTEXT_KEY)?
        .filter(|id| contexts.iter().any(|context| context.id == *id));
    Ok(stored_active)
}

fn get_stored_context_id(caller: &impl HostRuntimeApi, key: &str) -> Result<Option<Uuid>, String> {
    let response = caller
        .storage_get(&StorageGetRequest {
            key: key.to_string(),
        })
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

fn set_stored_context_id(
    caller: &impl HostRuntimeApi,
    key: &str,
    context_id: Option<Uuid>,
) -> Result<(), String> {
    let value = context_id.map_or_else(Vec::new, |id| id.to_string().into_bytes());
    caller
        .storage_set(&StorageSetRequest {
            key: key.to_string(),
            value,
        })
        .map_err(|error| error.to_string())
}

#[cfg(test)]
fn resolve_session_id(
    caller: &impl HostRuntimeApi,
    selector: ContextSelector,
) -> Result<Uuid, String> {
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    resolve_context_id_from_contexts(&contexts, &selector)
}

fn parse_selector(value: &str) -> Result<ContextSelector, String> {
    if let Ok(id) = Uuid::parse_str(value) {
        return Ok(ContextSelector::ById(id));
    }
    if value.trim().is_empty() {
        return Err("target must not be empty".to_string());
    }
    Ok(ContextSelector::ByName(value.to_string()))
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
    arguments
        .iter()
        .find(|argument| !argument.starts_with('-'))
        .cloned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListWindowsRequest {
    session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NewWindowRequest {
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KillWindowRequest {
    target: String,
    force_local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KillAllWindowsRequest {
    force_local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SwitchWindowRequest {
    target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WindowEntry {
    id: String,
    name: String,
    active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListWindowsResponse {
    windows: Vec<WindowEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WindowCommandAck {
    ok: bool,
    #[serde(default)]
    id: Option<String>,
}

bmux_plugin_sdk::export_plugin!(WindowsPlugin, include_str!("../plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin::ServiceCaller;
    use bmux_plugin_sdk::{
        ApiVersion, ContextCloseRequest, ContextCreateRequest, ContextListResponse,
        ContextSelectRequest, ContextSelectResponse, ContextSelector as SessionSelector,
        ContextSummary as SessionSummary, HostConnectionInfo, HostKernelBridge, HostMetadata,
        HostScope, NativeServiceContext, ProviderId, RegisteredService, ServiceKind,
        ServiceRequest, decode_service_message, encode_service_message,
    };
    use std::sync::Mutex;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct BridgeRequest {
        payload: Vec<u8>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct BridgeResponse {
        payload: Vec<u8>,
    }

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
        let request: bmux_ipc::Request = match bmux_ipc::decode(&bridge_request.payload) {
            Ok(request) => request,
            Err(_) => return 1,
        };

        let response = match request {
            bmux_ipc::Request::WhoAmI => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ClientIdentity {
                    id: Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111),
                })
            }
            bmux_ipc::Request::ListClients => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ClientList {
                    clients: vec![bmux_ipc::ClientSummary {
                        id: Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111),
                        selected_context_id: None,
                        selected_session_id: None,
                        following_client_id: None,
                        following_global: false,
                    }],
                })
            }
            bmux_ipc::Request::CreateContext {
                name: Some(name), ..
            } if name == "deny" => bmux_ipc::Response::Err(bmux_ipc::ErrorResponse {
                code: bmux_ipc::ErrorCode::InvalidRequest,
                message: "session policy denied for this operation".to_string(),
            }),
            bmux_ipc::Request::CreateContext { name, attributes } => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ContextCreated {
                    context: bmux_ipc::ContextSummary {
                        id: Uuid::new_v4(),
                        name,
                        attributes,
                    },
                })
            }
            bmux_ipc::Request::ListContexts => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ContextList {
                    contexts: vec![bmux_ipc::ContextSummary {
                        id: Uuid::new_v4(),
                        name: Some("alpha".to_string()),
                        attributes: BTreeMap::new(),
                    }],
                })
            }
            bmux_ipc::Request::CloseContext { selector, .. } => {
                if matches!(selector, bmux_ipc::ContextSelector::ByName(ref name) if name == "deny")
                {
                    bmux_ipc::Response::Err(bmux_ipc::ErrorResponse {
                        code: bmux_ipc::ErrorCode::InvalidRequest,
                        message: "session policy denied for this operation".to_string(),
                    })
                } else {
                    let id = match selector {
                        bmux_ipc::ContextSelector::ById(id) => id,
                        bmux_ipc::ContextSelector::ByName(_) => Uuid::new_v4(),
                    };
                    bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ContextClosed { id })
                }
            }
            bmux_ipc::Request::SelectContext { selector } => {
                let id = match selector {
                    bmux_ipc::ContextSelector::ById(id) => id,
                    bmux_ipc::ContextSelector::ByName(_) => Uuid::new_v4(),
                };
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ContextSelected {
                    context: bmux_ipc::ContextSummary {
                        id,
                        name: Some("selected".to_string()),
                        attributes: BTreeMap::new(),
                    },
                })
            }
            bmux_ipc::Request::CurrentContext => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::CurrentContext {
                    context: Some(bmux_ipc::ContextSummary {
                        id: Uuid::new_v4(),
                        name: Some("current".to_string()),
                        attributes: BTreeMap::new(),
                    }),
                })
            }
            _ => bmux_ipc::Response::Err(bmux_ipc::ErrorResponse {
                code: bmux_ipc::ErrorCode::InvalidRequest,
                message: "unsupported request in service bridge test".to_string(),
            }),
        };

        let encoded = match bmux_ipc::encode(&response) {
            Ok(encoded) => encoded,
            Err(_) => return 1,
        };
        let output = match encode_service_message(&BridgeResponse { payload: encoded }) {
            Ok(output) => output,
            Err(_) => return 1,
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
                interface_id: "context-query/v1".to_string(),
                provider: ProviderId::Host,
            },
            RegisteredService {
                capability: HostScope::new("bmux.contexts.write").expect("capability should parse"),
                kind: ServiceKind::Command,
                interface_id: "context-command/v1".to_string(),
                provider: ProviderId::Host,
            },
            RegisteredService {
                capability: HostScope::new("bmux.clients.read").expect("capability should parse"),
                kind: ServiceKind::Query,
                interface_id: "client-query/v1".to_string(),
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
        ];

        NativeServiceContext {
            plugin_id: "bmux.windows".to_string(),
            request: ServiceRequest {
                caller_plugin_id: "test.caller".to_string(),
                service: RegisteredService {
                    capability: HostScope::new(capability).expect("capability should parse"),
                    kind,
                    interface_id: interface_id.to_string(),
                    provider: ProviderId::Plugin("bmux.windows".to_string()),
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
                "bmux.windows.read".to_string(),
                "bmux.windows.write".to_string(),
            ],
            services: host_services,
            available_capabilities: vec![
                "bmux.contexts.read".to_string(),
                "bmux.contexts.write".to_string(),
                "bmux.clients.read".to_string(),
                "bmux.storage".to_string(),
            ],
            enabled_plugins: vec!["bmux.windows".to_string()],
            plugin_search_roots: vec!["/plugins".to_string()],
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: std::collections::BTreeMap::new(),
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
        creates: Mutex<Vec<Option<String>>>,
        kills: Mutex<Vec<ContextCloseRequest>>,
        selects: Mutex<Vec<Uuid>>,
        storage: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl MockHost {
        fn with_sessions(sessions: Vec<SessionSummary>) -> Self {
            Self {
                current_client_id: Uuid::new_v4(),
                selected_session_id: Mutex::new(sessions.first().map(|session| session.id)),
                sessions,
                fail_create: false,
                fail_kill: false,
                fail_current_client: false,
                creates: Mutex::new(Vec::new()),
                kills: Mutex::new(Vec::new()),
                selects: Mutex::new(Vec::new()),
                storage: Mutex::new(BTreeMap::new()),
            }
        }

        fn with_client_query_failure() -> Self {
            let sessions = sample_sessions();
            Self {
                current_client_id: Uuid::new_v4(),
                selected_session_id: Mutex::new(sessions.first().map(|session| session.id)),
                sessions,
                fail_create: false,
                fail_kill: false,
                fail_current_client: true,
                creates: Mutex::new(Vec::new()),
                kills: Mutex::new(Vec::new()),
                selects: Mutex::new(Vec::new()),
                storage: Mutex::new(BTreeMap::new()),
            }
        }

        fn with_failures(fail_create: bool, fail_kill: bool, _fail_pane_list: bool) -> Self {
            let sessions = sample_sessions();
            Self {
                current_client_id: Uuid::new_v4(),
                selected_session_id: Mutex::new(sessions.first().map(|session| session.id)),
                sessions,
                fail_create,
                fail_kill,
                fail_current_client: false,
                creates: Mutex::new(Vec::new()),
                kills: Mutex::new(Vec::new()),
                selects: Mutex::new(Vec::new()),
                storage: Mutex::new(BTreeMap::new()),
            }
        }
    }

    impl ServiceCaller for MockHost {
        fn call_service_raw(
            &self,
            _capability: &str,
            _kind: ServiceKind,
            interface_id: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            match (interface_id, operation) {
                ("context-query/v1", "list") => encode_service_message(&ContextListResponse {
                    contexts: self.sessions.clone(),
                }),
                ("context-command/v1", "create") => {
                    if self.fail_create {
                        return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                            details: "mock create failure".to_string(),
                        });
                    }
                    let request: ContextCreateRequest = decode_service_message(&payload)?;
                    self.creates
                        .lock()
                        .expect("create log lock should succeed")
                        .push(request.name.clone());
                    encode_service_message(&bmux_plugin_sdk::ContextCreateResponse {
                        context: SessionSummary {
                            id: Uuid::new_v4(),
                            name: request.name,
                            attributes: request.attributes,
                        },
                    })
                }
                ("context-command/v1", "close") => {
                    if self.fail_kill {
                        return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                            details: "mock kill failure".to_string(),
                        });
                    }
                    let request: ContextCloseRequest = decode_service_message(&payload)?;
                    self.kills
                        .lock()
                        .expect("kill log lock should succeed")
                        .push(request.clone());
                    encode_service_message(&bmux_plugin_sdk::ContextCloseResponse {
                        id: match request.selector {
                            SessionSelector::ById(id) => id,
                            SessionSelector::ByName(_) => Uuid::new_v4(),
                        },
                    })
                }
                ("context-command/v1", "select") => {
                    if self.fail_kill {
                        return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                            details: "mock select failure".to_string(),
                        });
                    }
                    let request: ContextSelectRequest = decode_service_message(&payload)?;
                    let selected = match request.selector {
                        SessionSelector::ById(id) => id,
                        SessionSelector::ByName(name) => self
                            .sessions
                            .iter()
                            .find(|session| session.name.as_deref() == Some(name.as_str()))
                            .map(|session| session.id)
                            .ok_or_else(|| bmux_plugin_sdk::PluginError::ServiceProtocol {
                                details: "mock select target not found".to_string(),
                            })?,
                    };
                    *self
                        .selected_session_id
                        .lock()
                        .expect("selected session lock should succeed") = Some(selected);
                    self.selects
                        .lock()
                        .expect("select log lock should succeed")
                        .push(selected);
                    encode_service_message(&ContextSelectResponse {
                        context: SessionSummary {
                            id: selected,
                            name: Some("selected".to_string()),
                            attributes: BTreeMap::new(),
                        },
                    })
                }
                ("context-query/v1", "current") => {
                    let current_context_id = *self
                        .selected_session_id
                        .lock()
                        .expect("selected context lock should succeed");
                    let context = current_context_id
                        .and_then(|id| self.sessions.iter().find(|entry| entry.id == id).cloned());
                    encode_service_message(&bmux_plugin_sdk::ContextCurrentResponse { context })
                }
                ("client-query/v1", "current") => {
                    if self.fail_current_client {
                        return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                            details: "mock current client failure".to_string(),
                        });
                    }
                    let selected_session_id = *self
                        .selected_session_id
                        .lock()
                        .expect("selected session lock should succeed");
                    encode_service_message(&bmux_plugin_sdk::CurrentClientResponse {
                        id: self.current_client_id,
                        selected_session_id,
                        following_client_id: None,
                        following_global: false,
                    })
                }
                ("storage-query/v1", "get") => {
                    let request: StorageGetRequest = decode_service_message(&payload)?;
                    let value = self
                        .storage
                        .lock()
                        .expect("storage lock should succeed")
                        .get(&request.key)
                        .cloned();
                    encode_service_message(&bmux_plugin_sdk::StorageGetResponse { value })
                }
                ("storage-command/v1", "set") => {
                    let request: StorageSetRequest = decode_service_message(&payload)?;
                    self.storage
                        .lock()
                        .expect("storage lock should succeed")
                        .insert(request.key, request.value);
                    encode_service_message(&())
                }
                _ => Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                    operation: "mock_service",
                }),
            }
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

    #[test]
    fn list_windows_projects_sessions_and_marks_first_active() {
        let host = MockHost::with_sessions(sample_sessions());
        let windows = list_windows(&host, None).expect("list should succeed");

        assert_eq!(windows.len(), 2);
        assert!(windows[0].active);
        assert!(!windows[1].active);
        assert_eq!(windows[0].name, "alpha");
        assert_eq!(windows[1].name, "beta");
    }

    #[test]
    fn list_windows_filters_by_session_selector() {
        let sessions = sample_sessions();
        let beta_id = sessions[1].id;
        let host = MockHost::with_sessions(sessions);

        let by_name = list_windows(&host, Some("beta")).expect("list by name should succeed");
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].name, "beta");

        let by_id =
            list_windows(&host, Some(&beta_id.to_string())).expect("list by id should succeed");
        assert_eq!(by_id.len(), 1);
        assert_eq!(by_id[0].id, beta_id.to_string());
    }

    #[test]
    fn resolve_session_id_finds_name_and_id() {
        let sessions = sample_sessions();
        let alpha_id = sessions[0].id;
        let host = MockHost::with_sessions(sessions);

        let resolved_name = resolve_session_id(&host, SessionSelector::ByName("alpha".to_string()))
            .expect("resolve by name should succeed");
        assert_eq!(resolved_name, alpha_id);

        let resolved_id = resolve_session_id(&host, SessionSelector::ById(alpha_id))
            .expect("resolve by id should succeed");
        assert_eq!(resolved_id, alpha_id);
    }

    #[test]
    fn parse_selector_rejects_blank_values() {
        let error = parse_selector("   ").expect_err("blank selector should fail");
        assert!(error.contains("must not be empty"));
    }

    #[test]
    fn create_window_calls_session_create() {
        let host = MockHost::with_sessions(sample_sessions());
        let ack = create_window(&host, Some("dev".to_string())).expect("create should succeed");
        assert!(ack.ok);
        assert!(ack.id.is_some());
        let creates = host.creates.lock().expect("create log lock should succeed");
        assert_eq!(creates.as_slice(), &[Some("dev".to_string())]);
    }

    #[test]
    fn kill_all_windows_calls_kill_for_each_session() {
        let host = MockHost::with_sessions(sample_sessions());
        let ack = kill_all_windows(&host, true).expect("kill all should succeed");
        assert!(ack.ok);
        let kills = host.kills.lock().expect("kill log lock should succeed");
        assert_eq!(kills.len(), 2);
        assert!(kills.iter().all(|request| request.force));
    }

    #[test]
    fn kill_window_passes_selector_and_force_local() {
        let host = MockHost::with_sessions(sample_sessions());
        let target = host
            .sessions
            .first()
            .expect("sample sessions should exist")
            .id;

        let ack =
            kill_window(&host, SessionSelector::ById(target), true).expect("kill should succeed");
        assert!(ack.ok);
        let target_text = target.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));

        let kills = host.kills.lock().expect("kill log lock should succeed");
        assert_eq!(kills.len(), 1);
        assert!(matches!(kills[0].selector, SessionSelector::ById(id) if id == target));
        assert!(kills[0].force);
    }

    #[test]
    fn switch_window_requires_target_context_to_exist() {
        let host = MockHost::with_sessions(sample_sessions());
        let mut last_selected_by_client = BTreeMap::new();
        let error = switch_window(
            &host,
            SessionSelector::ById(Uuid::new_v4()),
            &mut last_selected_by_client,
        )
        .expect_err("switch should fail when context is missing");
        assert!(error.contains("not found"));
    }

    #[test]
    fn switch_window_returns_selected_session_id() {
        let sessions = sample_sessions();
        let target_id = sessions[1].id;
        let host = MockHost::with_sessions(sessions);
        let mut last_selected_by_client = BTreeMap::new();

        let ack = switch_window(
            &host,
            SessionSelector::ById(target_id),
            &mut last_selected_by_client,
        )
        .expect("switch should succeed");
        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));

        let selects = host.selects.lock().expect("select log lock should succeed");
        assert_eq!(selects.as_slice(), &[target_id]);
    }

    #[test]
    fn switch_window_succeeds_when_current_client_query_fails() {
        let host = MockHost::with_client_query_failure();
        let target_id = host
            .sessions
            .get(1)
            .expect("sample sessions should include second item")
            .id;
        let mut last_selected_by_client = BTreeMap::new();

        let ack = switch_window(
            &host,
            SessionSelector::ById(target_id),
            &mut last_selected_by_client,
        )
        .expect("switch should succeed even if current client query fails");
        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));
    }

    #[test]
    fn next_window_selects_second_session() {
        let sessions = sample_sessions();
        let target_id = sessions[1].id;
        let host = MockHost::with_sessions(sessions);
        let mut last_selected_by_client = BTreeMap::new();

        let ack = cycle_window(
            &host,
            WindowCycleDirection::Next,
            &mut last_selected_by_client,
        )
        .expect("next window should succeed");
        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));
    }

    #[test]
    fn prev_window_selects_last_session() {
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
        let host = MockHost::with_sessions(sessions);
        let mut last_selected_by_client = BTreeMap::new();

        let ack = cycle_window(
            &host,
            WindowCycleDirection::Previous,
            &mut last_selected_by_client,
        )
        .expect("previous window should succeed");
        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));
    }

    #[test]
    fn last_window_requires_alternate_session() {
        let sessions = vec![SessionSummary {
            id: Uuid::new_v4(),
            name: Some("solo".to_string()),
            attributes: BTreeMap::new(),
        }];
        let host = MockHost::with_sessions(sessions);
        let mut last_selected_by_client = BTreeMap::new();
        let error = cycle_window(
            &host,
            WindowCycleDirection::Last,
            &mut last_selected_by_client,
        )
        .expect_err("last window should require alternate session");
        assert!(error.contains("no alternate window"));
    }

    #[test]
    fn last_window_selects_recorded_previous_session() {
        let sessions = sample_sessions();
        let target_id = sessions[0].id;
        let host = MockHost::with_sessions(sessions);
        let mut last_selected_by_client = BTreeMap::new();

        let _ = cycle_window(
            &host,
            WindowCycleDirection::Next,
            &mut last_selected_by_client,
        )
        .expect("next window should succeed");

        let ack = cycle_window(
            &host,
            WindowCycleDirection::Last,
            &mut last_selected_by_client,
        )
        .expect("last window should use remembered selection");

        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));
    }

    #[test]
    fn create_window_propagates_host_error() {
        let host = MockHost::with_failures(true, false, false);
        let error = create_window(&host, Some("dev".to_string()))
            .expect_err("create should surface host failure");
        assert!(error.contains("mock create failure"));
    }

    #[test]
    fn kill_window_propagates_host_error() {
        let host = MockHost::with_failures(false, true, false);
        let error = kill_window(&host, SessionSelector::ByName("alpha".to_string()), false)
            .expect_err("kill should surface host failure");
        assert!(error.contains("mock kill failure"));
    }

    #[test]
    fn kill_all_windows_propagates_host_error() {
        let host = MockHost::with_failures(false, true, false);
        let error = kill_all_windows(&host, true).expect_err("kill all should fail on host error");
        assert!(error.contains("mock kill failure"));
    }

    #[test]
    fn switch_window_propagates_context_select_error() {
        let host = MockHost::with_failures(false, true, false);
        let target = host
            .sessions
            .first()
            .expect("sample sessions should exist")
            .id;
        let mut last_selected_by_client = BTreeMap::new();
        let error = switch_window(
            &host,
            SessionSelector::ById(target),
            &mut last_selected_by_client,
        )
        .expect_err("switch should fail when select fails");
        assert!(error.contains("mock select failure"));
    }

    #[test]
    fn invoke_service_new_returns_ack_with_id() {
        let mut plugin = WindowsPlugin::default();
        let context = service_test_context(
            "window-command/v1",
            "new",
            encode_service_message(&NewWindowRequest {
                name: Some("ok".to_string()),
            })
            .expect("request should encode"),
            "bmux.windows.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );
        let ack: WindowCommandAck =
            decode_service_message(&response.payload).expect("ack should decode");
        assert!(ack.ok);
        assert!(ack.id.is_some());
    }

    #[test]
    fn invoke_service_new_surfaces_denied_error() {
        let mut plugin = WindowsPlugin::default();
        let context = service_test_context(
            "window-command/v1",
            "new",
            encode_service_message(&NewWindowRequest {
                name: Some("deny".to_string()),
            })
            .expect("request should encode"),
            "bmux.windows.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected service error");
        assert_eq!(error.code, "new_failed");
        assert!(error.message.contains("session policy denied"));
    }

    #[test]
    fn invoke_service_switch_returns_ack_with_selected_id() {
        let mut plugin = WindowsPlugin::default();
        let context = service_test_context(
            "window-command/v1",
            "switch",
            encode_service_message(&SwitchWindowRequest {
                target: "alpha".to_string(),
            })
            .expect("request should encode"),
            "bmux.windows.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );
        let ack: WindowCommandAck =
            decode_service_message(&response.payload).expect("ack should decode");
        assert!(ack.ok);
        assert!(ack.id.is_some_and(|id| !id.is_empty()));
    }

    #[test]
    fn invoke_service_rejects_invalid_payload() {
        let mut plugin = WindowsPlugin::default();
        let context = service_test_context(
            "window-command/v1",
            "kill",
            vec![1, 2, 3],
            "bmux.windows.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected service error");
        assert_eq!(error.code, "invalid_request");
    }

    #[test]
    fn invoke_service_kill_surfaces_denied_error() {
        let mut plugin = WindowsPlugin::default();
        let context = service_test_context(
            "window-command/v1",
            "kill",
            encode_service_message(&KillWindowRequest {
                target: "deny".to_string(),
                force_local: false,
            })
            .expect("request should encode"),
            "bmux.windows.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected kill failure");
        assert_eq!(error.code, "kill_failed");
        assert!(error.message.contains("session policy denied"));
    }

    #[test]
    fn invoke_service_rejects_unsupported_operation() {
        let mut plugin = WindowsPlugin::default();
        let context = service_test_context(
            "window-command/v1",
            "unknown",
            Vec::new(),
            "bmux.windows.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        let error = response
            .error
            .expect("expected unsupported operation error");
        assert_eq!(error.code, "unsupported_service_operation");
    }
}
