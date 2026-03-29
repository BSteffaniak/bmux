use crate::{
    CapabilityProvider, ContextCloseRequest, ContextCloseResponse, ContextCreateRequest,
    ContextCreateResponse, ContextCurrentResponse, ContextListResponse, ContextSelectRequest,
    ContextSelectResponse, ContextSelector as HostContextSelector,
    ContextSummary as HostContextSummary, CurrentClientResponse, DEFAULT_NATIVE_ACTIVATE_SYMBOL,
    DEFAULT_NATIVE_COMMAND_SYMBOL, DEFAULT_NATIVE_COMMAND_WITH_CONTEXT_SYMBOL,
    DEFAULT_NATIVE_DEACTIVATE_SYMBOL, DEFAULT_NATIVE_EVENT_SYMBOL, DEFAULT_NATIVE_SERVICE_SYMBOL,
    HostConnectionInfo, HostKernelBridge, HostKernelBridgeRequest, HostKernelBridgeResponse,
    HostMetadata, HostScope, LogWriteLevel, NativeCommandContext, NativeLifecycleContext,
    NativeServiceContext, PaneCloseRequest, PaneCloseResponse,
    PaneFocusDirection as HostPaneFocusDirection, PaneFocusRequest, PaneFocusResponse,
    PaneListRequest, PaneListResponse, PaneResizeRequest, PaneResizeResponse,
    PaneSelector as HostPaneSelector, PaneSplitDirection as HostPaneSplitDirection,
    PaneSplitRequest, PaneSplitResponse, PaneSummary as HostPaneSummary, PluginDeclaration,
    PluginEntrypoint, PluginError, PluginEvent, PluginRegistry, RecordingWriteEventRequest,
    RecordingWriteEventResponse, RegisteredPlugin, RegisteredService, Result, ServiceCaller,
    ServiceEnvelopeKind, ServiceKind, ServiceRequest, ServiceResponse, SessionCreateRequest,
    SessionCreateResponse, SessionKillRequest, SessionKillResponse, SessionListResponse,
    SessionSelectRequest, SessionSelectResponse, SessionSelector as HostSessionSelector,
    SessionSummary as HostSessionSummary, StaticPluginVtable, decode_service_envelope,
    decode_service_message, discover_registered_plugins_in_roots, encode_service_envelope,
    encode_service_message,
};
use bmux_ipc::{
    ContextSelector as IpcContextSelector, PaneFocusDirection as IpcPaneFocusDirection,
    PaneSelector as IpcPaneSelector, PaneSplitDirection as IpcPaneSplitDirection,
    Request as IpcRequest, Response as IpcResponse, ResponsePayload as IpcResponsePayload,
    SessionSelector as IpcSessionSelector,
};
use libloading::{Library, Symbol};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, error, info, trace, warn};

type PluginEntryFn = unsafe extern "C" fn() -> *const c_char;
type NativeRunCommandFn = unsafe extern "C" fn(*const c_char, usize, *const *const c_char) -> i32;
type NativeRunCommandWithContextFn = unsafe extern "C" fn(*const c_char) -> i32;
type NativeLifecycleFn = unsafe extern "C" fn(*const c_char) -> i32;
type NativeEventFn = unsafe extern "C" fn(*const c_char) -> i32;
type NativeInvokeServiceFn =
    unsafe extern "C" fn(*const u8, usize, *mut u8, usize, *mut usize) -> i32;

const NATIVE_SERVICE_STATUS_OK: i32 = 0;
const NATIVE_SERVICE_STATUS_BUFFER_TOO_SMALL: i32 = 4;
const KERNEL_STATUS_OK: i32 = 0;
const KERNEL_STATUS_BUFFER_TOO_SMALL: i32 = 4;

/// Backend that a [`LoadedPlugin`] uses to dispatch calls.
#[derive(Debug)]
enum PluginBackend {
    /// Dynamically loaded shared library (third-party / filesystem plugins).
    Dynamic(Library),
    /// Statically linked into the binary (bundled plugins behind feature flags).
    Static(StaticPluginVtable),
}

thread_local! {
    static COMMAND_OUTCOME_CAPTURE: RefCell<Option<crate::PluginCommandOutcome>> = const { RefCell::new(None) };
}

fn begin_command_outcome_capture() {
    COMMAND_OUTCOME_CAPTURE.with(|slot| {
        *slot.borrow_mut() = Some(crate::PluginCommandOutcome {
            effects: Vec::new(),
        });
    });
}

fn record_command_effect(effect: crate::PluginCommandEffect) {
    COMMAND_OUTCOME_CAPTURE.with(|slot| {
        if let Some(outcome) = slot.borrow_mut().as_mut() {
            outcome.effects.push(effect);
        }
    });
}

fn finish_command_outcome_capture() -> crate::PluginCommandOutcome {
    COMMAND_OUTCOME_CAPTURE
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or(crate::PluginCommandOutcome {
            effects: Vec::new(),
        })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CorePluginSettingsRequest {
    plugin_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CorePluginSettingsResponse {
    settings: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CoreStorageGetRequest {
    key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CoreStorageGetResponse {
    value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CoreStorageSetRequest {
    key: String,
    value: Vec<u8>,
}

impl ServiceCaller for NativeCommandContext {
    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        call_service_raw(
            &self.plugin_id,
            &self.required_capabilities,
            &self.provided_capabilities,
            &self.services,
            &self.available_capabilities,
            &self.enabled_plugins,
            &self.plugin_search_roots,
            &self.host,
            &self.connection,
            self.host_kernel_bridge,
            &self.plugin_settings_map,
            capability,
            kind,
            interface_id,
            operation,
            payload,
        )
    }
}

impl ServiceCaller for NativeLifecycleContext {
    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        call_service_raw(
            &self.plugin_id,
            &self.required_capabilities,
            &self.provided_capabilities,
            &self.services,
            &self.available_capabilities,
            &self.enabled_plugins,
            &self.plugin_search_roots,
            &self.host,
            &self.connection,
            self.host_kernel_bridge,
            &self.plugin_settings_map,
            capability,
            kind,
            interface_id,
            operation,
            payload,
        )
    }
}

impl ServiceCaller for NativeServiceContext {
    fn call_service_raw(
        &self,
        capability: &str,
        kind: ServiceKind,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let plugin_settings_map = deserialize_plugin_settings_map(&self.plugin_settings_map)?;
        call_service_raw(
            &self.plugin_id,
            &self.required_capabilities,
            &self.provided_capabilities,
            &self.services,
            &self.available_capabilities,
            &self.enabled_plugins,
            &self.plugin_search_roots,
            &self.host,
            &self.connection,
            self.host_kernel_bridge,
            &plugin_settings_map,
            capability,
            kind,
            interface_id,
            operation,
            payload,
        )
    }
}

/// Dispatch a host kernel bridge call for raw service bytes.
#[allow(dead_code)]
pub fn call_host_kernel_raw(context: &NativeServiceContext, payload: Vec<u8>) -> Result<Vec<u8>> {
    let Some(bridge) = context.host_kernel_bridge else {
        return Err(PluginError::UnsupportedHostOperation {
            operation: "call_host_kernel",
        });
    };
    let request = encode_service_message(&HostKernelBridgeRequest { payload })?;
    let mut output = vec![0u8; request.len().saturating_mul(4).max(1024)];
    let mut output_len = 0usize;

    let status = bridge.invoke(
        request.as_ptr(),
        request.len(),
        output.as_mut_ptr(),
        output.len(),
        &raw mut output_len,
    );

    if status == KERNEL_STATUS_BUFFER_TOO_SMALL {
        output.resize(output_len, 0);
        let status = bridge.invoke(
            request.as_ptr(),
            request.len(),
            output.as_mut_ptr(),
            output.len(),
            &raw mut output_len,
        );
        if status != KERNEL_STATUS_OK {
            return Err(PluginError::ServiceProtocol {
                details: format!("kernel bridge invocation failed with status {status}"),
            });
        }
    } else if status != KERNEL_STATUS_OK {
        return Err(PluginError::ServiceProtocol {
            details: format!("kernel bridge invocation failed with status {status}"),
        });
    }

    output.truncate(output_len);
    let response: HostKernelBridgeResponse = decode_service_message(&output)?;
    Ok(response.payload)
}

fn call_service_raw(
    caller_plugin_id: &str,
    required_capabilities: &[String],
    provided_capabilities: &[String],
    services: &[RegisteredService],
    available_capabilities: &[String],
    enabled_plugins: &[String],
    plugin_search_roots: &[String],
    host: &HostMetadata,
    connection: &HostConnectionInfo,
    host_kernel_bridge: Option<HostKernelBridge>,
    plugin_settings_map: &BTreeMap<String, toml::Value>,
    capability: &str,
    kind: ServiceKind,
    interface_id: &str,
    operation: &str,
    payload: Vec<u8>,
) -> Result<Vec<u8>> {
    let capability = HostScope::new(capability)?;
    let allowed = required_capabilities
        .iter()
        .chain(provided_capabilities.iter())
        .filter_map(|value| HostScope::new(value).ok())
        .any(|entry| entry == capability);
    if !allowed {
        return Err(PluginError::CapabilityAccessDenied {
            plugin_id: caller_plugin_id.to_string(),
            capability: capability.as_str().to_string(),
            operation: "call_service",
        });
    }

    let service = services
        .iter()
        .find(|service| {
            service.capability == capability
                && service.kind == kind
                && service.interface_id == interface_id
        })
        .cloned()
        .ok_or(PluginError::UnsupportedHostOperation {
            operation: "call_service",
        })?;

    if matches!(service.provider, crate::ProviderId::Host) {
        return handle_core_service_call(
            caller_plugin_id,
            connection,
            service,
            operation,
            payload,
            host_kernel_bridge,
            plugin_settings_map,
        );
    }

    let search_roots = plugin_search_roots
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let registry = discover_registered_plugins_in_roots(&search_roots)?;
    let provider_plugin_id = match &service.provider {
        crate::ProviderId::Plugin(plugin_id) => plugin_id.clone(),
        crate::ProviderId::Host => unreachable!("host services should be handled earlier"),
    };
    let registered =
        registry
            .get(&provider_plugin_id)
            .ok_or_else(|| PluginError::MissingServiceProvider {
                provider_plugin_id: provider_plugin_id.clone(),
                capability: service.capability.as_str().to_string(),
                interface_id: service.interface_id.clone(),
            })?;

    let available_capability_map = available_capabilities
        .iter()
        .filter_map(|value| HostScope::new(value).ok())
        .map(|capability| {
            let provider = CapabilityProvider {
                capability: capability.clone(),
                provider: crate::ProviderId::Host,
            };
            (capability, provider)
        })
        .collect::<BTreeMap<_, _>>();

    let loaded = load_registered_plugin(registered, host, &available_capability_map)?;
    let response = loaded.invoke_service(&NativeServiceContext {
        plugin_id: registered.declaration.id.as_str().to_string(),
        request: ServiceRequest {
            caller_plugin_id: caller_plugin_id.to_string(),
            service: service.clone(),
            operation: operation.to_string(),
            payload,
        },
        required_capabilities: registered
            .declaration
            .required_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        provided_capabilities: registered
            .declaration
            .provided_capabilities
            .iter()
            .map(ToString::to_string)
            .collect(),
        services: services.to_vec(),
        available_capabilities: available_capabilities.to_vec(),
        enabled_plugins: enabled_plugins.to_vec(),
        plugin_search_roots: plugin_search_roots.to_vec(),
        host: host.clone(),
        connection: connection.clone(),
        settings: serialize_plugin_settings(
            plugin_settings_map.get(registered.declaration.id.as_str()),
        ),
        plugin_settings_map: serialize_plugin_settings_map(plugin_settings_map),
        host_kernel_bridge,
    })?;

    if let Some(error) = response.error {
        return Err(PluginError::ServiceInvocationFailed {
            provider_plugin_id: service.provider.to_string(),
            capability: service.capability.as_str().to_string(),
            interface_id: service.interface_id,
            operation: operation.to_string(),
            code: error.code,
            message: error.message,
        });
    }

    Ok(response.payload)
}

fn serialize_plugin_settings(value: Option<&toml::Value>) -> BTreeMap<String, String> {
    value
        .and_then(toml::Value::as_table)
        .map(|table| {
            table
                .iter()
                .map(|(key, value)| (key.clone(), value.to_string()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default()
}

fn serialize_plugin_settings_map(
    plugin_settings_map: &BTreeMap<String, toml::Value>,
) -> BTreeMap<String, BTreeMap<String, String>> {
    plugin_settings_map
        .iter()
        .map(|(plugin_id, value)| (plugin_id.clone(), serialize_plugin_settings(Some(value))))
        .collect()
}

fn deserialize_plugin_settings_map(
    plugin_settings_map: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<BTreeMap<String, toml::Value>> {
    plugin_settings_map
        .iter()
        .map(|(plugin_id, settings)| {
            let table = settings
                .iter()
                .map(|(key, value)| {
                    toml::from_str::<toml::Value>(value)
                        .map(|parsed| (key.clone(), parsed))
                        .map_err(|error| PluginError::ServiceProtocol {
                            details: format!(
                                "failed parsing serialized setting '{key}' for plugin '{plugin_id}': {error}",
                            ),
                        })
                })
                .collect::<Result<toml::map::Map<_, _>>>()?;
            Ok((plugin_id.clone(), toml::Value::Table(table)))
        })
        .collect()
}

fn handle_core_service_call(
    caller_plugin_id: &str,
    connection: &HostConnectionInfo,
    service: RegisteredService,
    operation: &str,
    payload: Vec<u8>,
    host_kernel_bridge: Option<HostKernelBridge>,
    plugin_settings_map: &BTreeMap<String, toml::Value>,
) -> Result<Vec<u8>> {
    match (service.interface_id.as_str(), operation) {
        ("config-query/v1", "plugin_settings") => {
            let request: CorePluginSettingsRequest = decode_service_message(&payload)?;
            let settings = serialize_plugin_settings(plugin_settings_map.get(&request.plugin_id));
            encode_service_message(&CorePluginSettingsResponse { settings })
        }
        ("storage-query/v1", "get") => {
            let request: CoreStorageGetRequest = decode_service_message(&payload)?;
            validate_storage_key(&request.key)?;
            let path = storage_file_path(connection, caller_plugin_id, &request.key);
            let value = match fs::read(path) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(PluginError::ServiceProtocol {
                        details: format!("failed reading storage value: {error}"),
                    });
                }
            };
            encode_service_message(&CoreStorageGetResponse { value })
        }
        ("storage-command/v1", "set") => {
            let request: CoreStorageSetRequest = decode_service_message(&payload)?;
            validate_storage_key(&request.key)?;
            let path = storage_file_path(connection, caller_plugin_id, &request.key);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|error| PluginError::ServiceProtocol {
                    details: format!("failed creating storage directory: {error}"),
                })?;
            }
            fs::write(path, request.value).map_err(|error| PluginError::ServiceProtocol {
                details: format!("failed writing storage value: {error}"),
            })?;
            encode_service_message(&())
        }
        ("logging-command/v1", "write") => {
            let request: crate::LogWriteRequest = decode_service_message(&payload)?;
            emit_plugin_log(caller_plugin_id, &request)?;
            encode_service_message(&())
        }
        ("recording-command/v1", "write_event") => {
            let request: RecordingWriteEventRequest = decode_service_message(&payload)?;
            let response = execute_kernel_request(
                host_kernel_bridge,
                IpcRequest::RecordingWriteCustomEvent {
                    session_id: request.session_id,
                    pane_id: request.pane_id,
                    source: caller_plugin_id.to_string(),
                    name: request.name,
                    payload: serde_json::to_vec(&request.payload).unwrap_or_default(),
                },
            )?;
            match response {
                IpcResponsePayload::RecordingCustomEventWritten { accepted } => {
                    encode_service_message(&RecordingWriteEventResponse { accepted })
                }
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for recording-command/v1:write_event"
                        .to_string(),
                }),
            }
        }
        ("session-query/v1", "list") => {
            let response = execute_kernel_request(host_kernel_bridge, IpcRequest::ListSessions)?;
            match response {
                IpcResponsePayload::SessionList { sessions } => {
                    let sessions = sessions
                        .into_iter()
                        .map(|entry| HostSessionSummary {
                            id: entry.id,
                            name: entry.name,
                            client_count: entry.client_count,
                        })
                        .collect();
                    encode_service_message(&SessionListResponse { sessions })
                }
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for session-query/v1:list".to_string(),
                }),
            }
        }
        ("client-query/v1", "current") => {
            let client_id = match execute_kernel_request(host_kernel_bridge, IpcRequest::WhoAmI)? {
                IpcResponsePayload::ClientIdentity { id } => id,
                _ => {
                    return Err(PluginError::ServiceProtocol {
                        details: "unexpected response payload for client-query/v1:current whoami"
                            .to_string(),
                    });
                }
            };
            let response = execute_kernel_request(host_kernel_bridge, IpcRequest::ListClients)?;
            match response {
                IpcResponsePayload::ClientList { clients } => {
                    let current = clients.into_iter().find(|entry| entry.id == client_id);
                    encode_service_message(&CurrentClientResponse {
                        id: client_id,
                        selected_session_id: current
                            .as_ref()
                            .and_then(|entry| entry.selected_session_id),
                        following_client_id: current
                            .as_ref()
                            .and_then(|entry| entry.following_client_id),
                        following_global: current
                            .as_ref()
                            .is_some_and(|entry| entry.following_global),
                    })
                }
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for client-query/v1:current list-clients"
                        .to_string(),
                }),
            }
        }
        ("context-query/v1", "list") => {
            let response = execute_kernel_request(host_kernel_bridge, IpcRequest::ListContexts)?;
            match response {
                IpcResponsePayload::ContextList { contexts } => {
                    let contexts = contexts
                        .into_iter()
                        .map(|entry| HostContextSummary {
                            id: entry.id,
                            name: entry.name,
                            attributes: entry.attributes,
                        })
                        .collect();
                    encode_service_message(&ContextListResponse { contexts })
                }
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for context-query/v1:list".to_string(),
                }),
            }
        }
        ("context-query/v1", "current") => {
            let response = execute_kernel_request(host_kernel_bridge, IpcRequest::CurrentContext)?;
            match response {
                IpcResponsePayload::CurrentContext { context } => {
                    let context = context.map(|entry| HostContextSummary {
                        id: entry.id,
                        name: entry.name,
                        attributes: entry.attributes,
                    });
                    encode_service_message(&ContextCurrentResponse { context })
                }
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for context-query/v1:current".to_string(),
                }),
            }
        }
        ("context-command/v1", "create") => {
            let request: ContextCreateRequest = decode_service_message(&payload)?;
            let response = execute_kernel_request(
                host_kernel_bridge,
                IpcRequest::CreateContext {
                    name: request.name,
                    attributes: request.attributes,
                },
            )?;
            match response {
                IpcResponsePayload::ContextCreated { context } => {
                    record_command_effect(crate::PluginCommandEffect::SelectContext {
                        context_id: context.id,
                    });
                    encode_service_message(&ContextCreateResponse {
                        context: HostContextSummary {
                            id: context.id,
                            name: context.name,
                            attributes: context.attributes,
                        },
                    })
                }
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for context-command/v1:create"
                        .to_string(),
                }),
            }
        }
        ("context-command/v1", "select") => {
            let request: ContextSelectRequest = decode_service_message(&payload)?;
            let response = execute_kernel_request(
                host_kernel_bridge,
                IpcRequest::SelectContext {
                    selector: context_selector_to_ipc(request.selector),
                },
            )?;
            match response {
                IpcResponsePayload::ContextSelected { context } => {
                    record_command_effect(crate::PluginCommandEffect::SelectContext {
                        context_id: context.id,
                    });
                    encode_service_message(&ContextSelectResponse {
                        context: HostContextSummary {
                            id: context.id,
                            name: context.name,
                            attributes: context.attributes,
                        },
                    })
                }
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for context-command/v1:select"
                        .to_string(),
                }),
            }
        }
        ("context-command/v1", "close") => {
            let request: ContextCloseRequest = decode_service_message(&payload)?;
            let response = execute_kernel_request(
                host_kernel_bridge,
                IpcRequest::CloseContext {
                    selector: context_selector_to_ipc(request.selector),
                    force: request.force,
                },
            )?;
            match response {
                IpcResponsePayload::ContextClosed { id } => {
                    encode_service_message(&ContextCloseResponse { id })
                }
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for context-command/v1:close".to_string(),
                }),
            }
        }
        ("session-command/v1", "new") => {
            let request: SessionCreateRequest = decode_service_message(&payload)?;
            let response = execute_kernel_request(
                host_kernel_bridge,
                IpcRequest::NewSession { name: request.name },
            )?;
            match response {
                IpcResponsePayload::SessionCreated { id, name } => {
                    encode_service_message(&SessionCreateResponse { id, name })
                }
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for session-command/v1:new".to_string(),
                }),
            }
        }
        ("session-command/v1", "kill") => {
            let request: SessionKillRequest = decode_service_message(&payload)?;
            let response = execute_kernel_request(
                host_kernel_bridge,
                IpcRequest::KillSession {
                    selector: session_selector_to_ipc(request.selector),
                    force_local: request.force_local,
                },
            )?;
            match response {
                IpcResponsePayload::SessionKilled { id } => {
                    encode_service_message(&SessionKillResponse { id })
                }
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for session-command/v1:kill".to_string(),
                }),
            }
        }
        ("session-command/v1", "select") => {
            let request: SessionSelectRequest = decode_service_message(&payload)?;
            let response = execute_kernel_request(
                host_kernel_bridge,
                IpcRequest::Attach {
                    selector: session_selector_to_ipc(request.selector),
                },
            )?;
            match response {
                IpcResponsePayload::Attached { grant } => {
                    encode_service_message(&SessionSelectResponse {
                        session_id: grant.session_id,
                        attach_token: grant.attach_token,
                        expires_at_epoch_ms: grant.expires_at_epoch_ms,
                    })
                }
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for session-command/v1:select"
                        .to_string(),
                }),
            }
        }
        ("pane-query/v1", "list") => {
            let request: PaneListRequest = decode_service_message(&payload)?;
            let response = execute_kernel_request(
                host_kernel_bridge,
                IpcRequest::ListPanes {
                    session: request.session.map(session_selector_to_ipc),
                },
            )?;
            match response {
                IpcResponsePayload::PaneList { panes } => {
                    let panes = panes
                        .into_iter()
                        .map(|entry| HostPaneSummary {
                            id: entry.id,
                            index: entry.index,
                            name: entry.name,
                            focused: entry.focused,
                        })
                        .collect();
                    encode_service_message(&PaneListResponse { panes })
                }
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for pane-query/v1:list".to_string(),
                }),
            }
        }
        ("pane-command/v1", "split") => {
            let request: PaneSplitRequest = decode_service_message(&payload)?;
            let response = execute_kernel_request(
                host_kernel_bridge,
                IpcRequest::SplitPane {
                    session: request.session.map(session_selector_to_ipc),
                    target: request.target.map(pane_selector_to_ipc),
                    direction: pane_split_direction_to_ipc(request.direction),
                    ratio_pct: None,
                },
            )?;
            match response {
                IpcResponsePayload::PaneSplit { id, session_id } => {
                    encode_service_message(&PaneSplitResponse { id, session_id })
                }
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for pane-command/v1:split".to_string(),
                }),
            }
        }
        ("pane-command/v1", "focus") => {
            let request: PaneFocusRequest = decode_service_message(&payload)?;
            let response = execute_kernel_request(
                host_kernel_bridge,
                IpcRequest::FocusPane {
                    session: request.session.map(session_selector_to_ipc),
                    target: request.target.map(pane_selector_to_ipc),
                    direction: request.direction.map(pane_focus_direction_to_ipc),
                },
            )?;
            match response {
                IpcResponsePayload::PaneFocused { id, session_id } => {
                    encode_service_message(&PaneFocusResponse { id, session_id })
                }
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for pane-command/v1:focus".to_string(),
                }),
            }
        }
        ("pane-command/v1", "resize") => {
            let request: PaneResizeRequest = decode_service_message(&payload)?;
            let response = execute_kernel_request(
                host_kernel_bridge,
                IpcRequest::ResizePane {
                    session: request.session.map(session_selector_to_ipc),
                    target: request.target.map(pane_selector_to_ipc),
                    delta: request.delta,
                },
            )?;
            match response {
                IpcResponsePayload::PaneResized { session_id } => {
                    encode_service_message(&PaneResizeResponse { session_id })
                }
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for pane-command/v1:resize".to_string(),
                }),
            }
        }
        ("pane-command/v1", "close") => {
            let request: PaneCloseRequest = decode_service_message(&payload)?;
            let response = execute_kernel_request(
                host_kernel_bridge,
                IpcRequest::ClosePane {
                    session: request.session.map(session_selector_to_ipc),
                    target: request.target.map(pane_selector_to_ipc),
                },
            )?;
            match response {
                IpcResponsePayload::PaneClosed {
                    id,
                    session_id,
                    session_closed,
                } => encode_service_message(&PaneCloseResponse {
                    id,
                    session_id,
                    session_closed,
                }),
                _ => Err(PluginError::ServiceProtocol {
                    details: "unexpected response payload for pane-command/v1:close".to_string(),
                }),
            }
        }
        _ => Err(PluginError::UnsupportedHostOperation {
            operation: "call_service",
        }),
    }
}

fn emit_plugin_log(caller_plugin_id: &str, request: &crate::LogWriteRequest) -> Result<()> {
    let requested_target = request
        .target
        .as_deref()
        .filter(|entry| !entry.trim().is_empty())
        .unwrap_or(caller_plugin_id);
    let message = request.message.trim();
    if message.is_empty() {
        return Err(PluginError::ServiceProtocol {
            details: "log message cannot be empty".to_string(),
        });
    }

    match request.level {
        LogWriteLevel::Error => {
            error!(
                plugin_id = caller_plugin_id,
                plugin_target = requested_target,
                "{}",
                request.message
            );
        }
        LogWriteLevel::Warn => {
            warn!(
                plugin_id = caller_plugin_id,
                plugin_target = requested_target,
                "{}",
                request.message
            );
        }
        LogWriteLevel::Info => {
            info!(
                plugin_id = caller_plugin_id,
                plugin_target = requested_target,
                "{}",
                request.message
            );
        }
        LogWriteLevel::Debug => {
            debug!(
                plugin_id = caller_plugin_id,
                plugin_target = requested_target,
                "{}",
                request.message
            );
        }
        LogWriteLevel::Trace => {
            trace!(
                plugin_id = caller_plugin_id,
                plugin_target = requested_target,
                "{}",
                request.message
            );
        }
    }

    Ok(())
}

fn session_selector_to_ipc(selector: HostSessionSelector) -> IpcSessionSelector {
    match selector {
        HostSessionSelector::ById(id) => IpcSessionSelector::ById(id),
        HostSessionSelector::ByName(name) => IpcSessionSelector::ByName(name),
    }
}

fn context_selector_to_ipc(selector: HostContextSelector) -> IpcContextSelector {
    match selector {
        HostContextSelector::ById(id) => IpcContextSelector::ById(id),
        HostContextSelector::ByName(name) => IpcContextSelector::ByName(name),
    }
}

const fn pane_selector_to_ipc(selector: HostPaneSelector) -> IpcPaneSelector {
    match selector {
        HostPaneSelector::ById(id) => IpcPaneSelector::ById(id),
        HostPaneSelector::ByIndex(index) => IpcPaneSelector::ByIndex(index),
        HostPaneSelector::Active => IpcPaneSelector::Active,
    }
}

const fn pane_split_direction_to_ipc(direction: HostPaneSplitDirection) -> IpcPaneSplitDirection {
    match direction {
        HostPaneSplitDirection::Vertical => IpcPaneSplitDirection::Vertical,
        HostPaneSplitDirection::Horizontal => IpcPaneSplitDirection::Horizontal,
    }
}

const fn pane_focus_direction_to_ipc(direction: HostPaneFocusDirection) -> IpcPaneFocusDirection {
    match direction {
        HostPaneFocusDirection::Next => IpcPaneFocusDirection::Next,
        HostPaneFocusDirection::Prev => IpcPaneFocusDirection::Prev,
    }
}

fn execute_kernel_request(
    host_kernel_bridge: Option<HostKernelBridge>,
    request: IpcRequest,
) -> Result<IpcResponsePayload> {
    let bridge = host_kernel_bridge.ok_or(PluginError::UnsupportedHostOperation {
        operation: "call_host_kernel",
    })?;
    let encoded_request =
        bmux_ipc::encode(&request).map_err(|error| PluginError::ServiceProtocol {
            details: format!("failed encoding kernel request: {error}"),
        })?;
    let encoded_response = invoke_host_kernel_bridge(bridge, encoded_request)?;
    let response: IpcResponse =
        bmux_ipc::decode(&encoded_response).map_err(|error| PluginError::ServiceProtocol {
            details: format!("failed decoding kernel response: {error}"),
        })?;
    match response {
        IpcResponse::Ok(payload) => Ok(payload),
        IpcResponse::Err(error) => Err(PluginError::ServiceProtocol {
            details: format!("kernel request failed: {}", error.message),
        }),
    }
}

fn invoke_host_kernel_bridge(bridge: HostKernelBridge, payload: Vec<u8>) -> Result<Vec<u8>> {
    let request = encode_service_message(&HostKernelBridgeRequest { payload })?;
    let mut output = vec![0u8; request.len().saturating_mul(4).max(1024)];
    let mut output_len = 0usize;

    let status = bridge.invoke(
        request.as_ptr(),
        request.len(),
        output.as_mut_ptr(),
        output.len(),
        &raw mut output_len,
    );

    if status == KERNEL_STATUS_BUFFER_TOO_SMALL {
        output.resize(output_len, 0);
        let status = bridge.invoke(
            request.as_ptr(),
            request.len(),
            output.as_mut_ptr(),
            output.len(),
            &raw mut output_len,
        );
        if status != KERNEL_STATUS_OK {
            return Err(PluginError::ServiceProtocol {
                details: format!("kernel bridge invocation failed with status {status}"),
            });
        }
    } else if status != KERNEL_STATUS_OK {
        return Err(PluginError::ServiceProtocol {
            details: format!("kernel bridge invocation failed with status {status}"),
        });
    }

    output.truncate(output_len);
    let response: HostKernelBridgeResponse = decode_service_message(&output)?;
    Ok(response.payload)
}

fn storage_file_path(connection: &HostConnectionInfo, plugin_id: &str, key: &str) -> PathBuf {
    PathBuf::from(&connection.data_dir)
        .join("plugin-storage")
        .join(plugin_id)
        .join(format!("{key}.bin"))
}

fn validate_storage_key(key: &str) -> Result<()> {
    if key.is_empty()
        || !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(PluginError::ServiceProtocol {
            details: "storage key must be non-empty and use [A-Za-z0-9._-]".to_string(),
        });
    }
    Ok(())
}

pub struct LoadedPlugin {
    pub registered: RegisteredPlugin,
    pub declaration: PluginDeclaration,
    backend: PluginBackend,
}

impl LoadedPlugin {
    #[must_use]
    pub fn commands(&self) -> &[crate::PluginCommand] {
        &self.declaration.commands
    }

    #[must_use]
    pub fn supports_command(&self, command_name: &str) -> bool {
        self.declaration
            .commands
            .iter()
            .any(|command| command.name == command_name)
    }

    /// # Errors
    ///
    /// Returns an error when the plugin does not declare the command, the
    /// command symbol cannot be loaded, or any command input contains an
    /// interior NUL byte.
    ///
    /// **Note:** Static plugins always require a command context.  Calling
    /// this method (which passes `context: None`) on a static plugin will
    /// return [`PluginError::NativeCommandSymbol`].  Use
    /// [`run_command_with_context`](Self::run_command_with_context) instead.
    pub fn run_command(&self, command_name: &str, arguments: &[String]) -> Result<i32> {
        self.run_command_with_context(command_name, arguments, None)
    }

    /// # Errors
    ///
    /// Returns an error when the plugin does not declare the command, the
    /// command symbol cannot be loaded, or any command input contains an
    /// interior NUL byte.
    pub fn run_command_with_context(
        &self,
        command_name: &str,
        arguments: &[String],
        context: Option<&NativeCommandContext>,
    ) -> Result<i32> {
        let (status, _) =
            self.run_command_with_context_and_outcome(command_name, arguments, context)?;
        Ok(status)
    }

    /// # Errors
    ///
    /// Returns an error when the plugin does not declare the command, the
    /// command symbol cannot be loaded, or any command input contains an
    /// interior NUL byte.
    pub fn run_command_with_context_and_outcome(
        &self,
        command_name: &str,
        arguments: &[String],
        context: Option<&NativeCommandContext>,
    ) -> Result<(i32, crate::PluginCommandOutcome)> {
        if !self.supports_command(command_name) {
            return Err(PluginError::UnknownPluginCommand {
                plugin_id: self.declaration.id.as_str().to_string(),
                command: command_name.to_string(),
            });
        }

        if let Some(context) = context {
            let payload = CString::new(
                serde_json::to_string(context).expect("native command context should serialize"),
            )
            .map_err(|_| PluginError::InvalidNativeCommandInput {
                plugin_id: self.declaration.id.as_str().to_string(),
                field: "context",
            })?;

            match &self.backend {
                PluginBackend::Static(vtable) => {
                    begin_command_outcome_capture();
                    let status = (vtable.run_command_with_context)(payload.as_ptr());
                    let outcome = finish_command_outcome_capture();
                    return Ok((status, outcome));
                }
                PluginBackend::Dynamic(library) => {
                    if let Ok(command_symbol) = unsafe {
                        library.get::<NativeRunCommandWithContextFn>(
                            DEFAULT_NATIVE_COMMAND_WITH_CONTEXT_SYMBOL.as_bytes(),
                        )
                    } {
                        begin_command_outcome_capture();
                        let status = unsafe { command_symbol(payload.as_ptr()) };
                        let outcome = finish_command_outcome_capture();
                        return Ok((status, outcome));
                    }
                }
            }
        }

        // Fallback: use the legacy run_command symbol (dynamic only)
        let PluginBackend::Dynamic(library) = &self.backend else {
            return Err(PluginError::NativeCommandSymbol {
                plugin_id: self.declaration.id.as_str().to_string(),
                symbol: DEFAULT_NATIVE_COMMAND_SYMBOL.to_string(),
                details: "static plugins require context-based command dispatch".to_string(),
            });
        };

        let command_name =
            CString::new(command_name).map_err(|_| PluginError::InvalidNativeCommandInput {
                plugin_id: self.declaration.id.as_str().to_string(),
                field: "command_name",
            })?;
        let argument_values = arguments
            .iter()
            .map(|argument| {
                CString::new(argument.as_str()).map_err(|_| {
                    PluginError::InvalidNativeCommandInput {
                        plugin_id: self.declaration.id.as_str().to_string(),
                        field: "arguments",
                    }
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let argument_ptrs = argument_values
            .iter()
            .map(|value| value.as_ptr())
            .collect::<Vec<_>>();

        let command_symbol: Symbol<'_, NativeRunCommandFn> = unsafe {
            library.get(DEFAULT_NATIVE_COMMAND_SYMBOL.as_bytes())
        }
        .map_err(|error| PluginError::NativeCommandSymbol {
            plugin_id: self.declaration.id.as_str().to_string(),
            symbol: DEFAULT_NATIVE_COMMAND_SYMBOL.to_string(),
            details: error.to_string(),
        })?;

        let status = unsafe {
            command_symbol(
                command_name.as_ptr(),
                argument_ptrs.len(),
                argument_ptrs.as_ptr(),
            )
        };

        Ok((
            status,
            crate::PluginCommandOutcome {
                effects: Vec::new(),
            },
        ))
    }

    /// # Errors
    ///
    /// Returns an error when the lifecycle symbol cannot be loaded or the
    /// lifecycle payload cannot be encoded.
    pub fn activate(&self, context: &NativeLifecycleContext) -> Result<i32> {
        self.run_lifecycle_symbol(DEFAULT_NATIVE_ACTIVATE_SYMBOL, context)
    }

    /// # Errors
    ///
    /// Returns an error when the lifecycle symbol cannot be loaded or the
    /// lifecycle payload cannot be encoded.
    pub fn deactivate(&self, context: &NativeLifecycleContext) -> Result<i32> {
        self.run_lifecycle_symbol(DEFAULT_NATIVE_DEACTIVATE_SYMBOL, context)
    }

    #[must_use]
    pub fn receives_event(&self, event: &PluginEvent) -> bool {
        self.declaration.event_subscriptions.is_empty()
            || self
                .declaration
                .event_subscriptions
                .iter()
                .any(|subscription| subscription.matches(event))
    }

    /// # Errors
    ///
    /// Returns an error when the event symbol cannot be loaded or the event
    /// payload cannot be encoded.
    pub fn dispatch_event(&self, event: &PluginEvent) -> Result<Option<i32>> {
        if !self.receives_event(event) {
            return Ok(None);
        }

        let payload = CString::new(
            serde_json::to_string(event).expect("plugin event payload should serialize"),
        )
        .map_err(|_| PluginError::InvalidNativeEventInput {
            plugin_id: self.declaration.id.as_str().to_string(),
        })?;

        let status = match &self.backend {
            PluginBackend::Static(vtable) => (vtable.handle_event)(payload.as_ptr()),
            PluginBackend::Dynamic(library) => {
                let event_symbol: Symbol<'_, NativeEventFn> =
                    unsafe { library.get(DEFAULT_NATIVE_EVENT_SYMBOL.as_bytes()) }.map_err(
                        |error| PluginError::NativeEventSymbol {
                            plugin_id: self.declaration.id.as_str().to_string(),
                            symbol: DEFAULT_NATIVE_EVENT_SYMBOL.to_string(),
                            details: error.to_string(),
                        },
                    )?;
                unsafe { event_symbol(payload.as_ptr()) }
            }
        };

        Ok(Some(status))
    }

    /// # Errors
    ///
    /// Returns an error when the service symbol cannot be loaded, the service
    /// payload cannot be encoded, or the plugin returns invalid transport data.
    pub fn invoke_service(&self, context: &NativeServiceContext) -> Result<ServiceResponse> {
        let payload = encode_service_envelope(0, ServiceEnvelopeKind::Request, context)?;

        // For dynamic backends, resolve the symbol once up-front so we can
        // return a proper error instead of panicking inside the closure.
        let resolved_symbol = match &self.backend {
            PluginBackend::Static(_) => None,
            PluginBackend::Dynamic(library) => {
                let sym: Symbol<'_, NativeInvokeServiceFn> =
                    unsafe { library.get(DEFAULT_NATIVE_SERVICE_SYMBOL.as_bytes()) }.map_err(
                        |error| PluginError::NativeServiceSymbol {
                            plugin_id: self.declaration.id.as_str().to_string(),
                            symbol: DEFAULT_NATIVE_SERVICE_SYMBOL.to_string(),
                            details: error.to_string(),
                        },
                    )?;
                Some(sym)
            }
        };

        let call_service = |payload: &[u8], output: &mut [u8], output_len: &mut usize| -> i32 {
            match &self.backend {
                PluginBackend::Static(vtable) => (vtable.invoke_service)(
                    payload.as_ptr(),
                    payload.len(),
                    output.as_mut_ptr(),
                    output.len(),
                    output_len,
                ),
                PluginBackend::Dynamic(_) => {
                    let service_fn = resolved_symbol
                        .as_ref()
                        .expect("resolved_symbol is Some for Dynamic backend");
                    unsafe {
                        service_fn(
                            payload.as_ptr(),
                            payload.len(),
                            output.as_mut_ptr(),
                            output.len(),
                            output_len,
                        )
                    }
                }
            }
        };

        let mut output = vec![0_u8; 4096];
        let mut output_len = 0_usize;
        let mut status = call_service(&payload, &mut output, &mut output_len);
        if status == NATIVE_SERVICE_STATUS_BUFFER_TOO_SMALL {
            output.resize(output_len.max(output.len() * 2), 0);
            status = call_service(&payload, &mut output, &mut output_len);
        }

        if status != NATIVE_SERVICE_STATUS_OK {
            return Err(PluginError::NativeServiceInvocation {
                plugin_id: self.declaration.id.as_str().to_string(),
                status,
            });
        }

        if output_len > output.len() {
            return Err(PluginError::InvalidNativeServiceOutput {
                plugin_id: self.declaration.id.as_str().to_string(),
                details: format!(
                    "service returned {output_len} bytes into {} byte buffer",
                    output.len(),
                ),
            });
        }
        output.truncate(output_len);

        let (_, response) =
            decode_service_envelope::<ServiceResponse>(&output, ServiceEnvelopeKind::Response)?;
        Ok(response)
    }

    fn run_lifecycle_symbol(&self, symbol: &str, context: &NativeLifecycleContext) -> Result<i32> {
        let payload = CString::new(
            serde_json::to_string(context).expect("native lifecycle context should serialize"),
        )
        .map_err(|_| PluginError::InvalidNativeLifecycleInput {
            plugin_id: self.declaration.id.as_str().to_string(),
        })?;

        let status = match &self.backend {
            PluginBackend::Static(vtable) => {
                let func = if symbol == DEFAULT_NATIVE_ACTIVATE_SYMBOL {
                    vtable.activate
                } else if symbol == DEFAULT_NATIVE_DEACTIVATE_SYMBOL {
                    vtable.deactivate
                } else {
                    return Err(PluginError::NativeLifecycleSymbol {
                        plugin_id: self.declaration.id.as_str().to_string(),
                        symbol: symbol.to_string(),
                        details: "unknown lifecycle symbol for static plugin".to_string(),
                    });
                };
                func(payload.as_ptr())
            }
            PluginBackend::Dynamic(library) => {
                let lifecycle_symbol: Symbol<'_, NativeLifecycleFn> = unsafe {
                    library.get(symbol.as_bytes())
                }
                .map_err(|error| PluginError::NativeLifecycleSymbol {
                    plugin_id: self.declaration.id.as_str().to_string(),
                    symbol: symbol.to_string(),
                    details: error.to_string(),
                })?;
                unsafe { lifecycle_symbol(payload.as_ptr()) }
            }
        };

        Ok(status)
    }
}

#[derive(Debug, Default)]
pub struct NativePluginLoader;

impl NativePluginLoader {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// # Errors
    ///
    /// Returns an error when the plugin is incompatible, missing, fails to load,
    /// or returns a descriptor that conflicts with its manifest.
    pub fn load_registered_plugin(
        &self,
        registered_plugin: &RegisteredPlugin,
        host: &HostMetadata,
        available_capabilities: &BTreeMap<HostScope, crate::CapabilityProvider>,
    ) -> Result<LoadedPlugin> {
        PluginRegistry::validate_registered_plugin(
            registered_plugin,
            host,
            available_capabilities,
        )?;

        let entry_path = registered_plugin
            .manifest
            .resolve_entry_path(
                registered_plugin
                    .manifest_path
                    .parent()
                    .unwrap_or_else(|| Path::new(".")),
            )
            .ok_or_else(|| PluginError::MissingEntryPath {
                plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            })?;
        let library = unsafe { Library::new(&entry_path) }.map_err(|error| {
            PluginError::NativeLibraryLoad {
                plugin_id: registered_plugin.declaration.id.as_str().to_string(),
                path: entry_path.clone(),
                details: error.to_string(),
            }
        })?;

        let declaration = load_native_declaration(&library, registered_plugin)?;
        PluginRegistry::validate_registered_plugin(
            &RegisteredPlugin {
                declaration: declaration.clone(),
                ..registered_plugin.clone()
            },
            host,
            available_capabilities,
        )?;
        compare_manifest_and_embedded(registered_plugin, &declaration)?;

        Ok(LoadedPlugin {
            registered: registered_plugin.clone(),
            declaration,
            backend: PluginBackend::Dynamic(library),
        })
    }
}

/// # Errors
///
/// Returns an error when the plugin cannot be loaded.
pub fn load_registered_plugin(
    registered_plugin: &RegisteredPlugin,
    host: &HostMetadata,
    available_capabilities: &BTreeMap<HostScope, crate::CapabilityProvider>,
) -> Result<LoadedPlugin> {
    NativePluginLoader::new().load_registered_plugin(
        registered_plugin,
        host,
        available_capabilities,
    )
}

/// Load a statically-linked bundled plugin from its vtable.
///
/// This bypasses filesystem discovery and `dlopen` entirely.  The plugin's
/// manifest TOML is obtained by calling the vtable's `entry` function pointer
/// directly, and the resulting [`LoadedPlugin`] dispatches all subsequent calls
/// through the same vtable.
///
/// # Errors
///
/// Returns an error when the manifest cannot be parsed or validated.
pub fn load_static_plugin(
    registered_plugin: &RegisteredPlugin,
    vtable: StaticPluginVtable,
    host: &HostMetadata,
    available_capabilities: &BTreeMap<HostScope, crate::CapabilityProvider>,
) -> Result<LoadedPlugin> {
    let manifest_ptr = (vtable.entry)();
    if manifest_ptr.is_null() {
        return Err(PluginError::NullPluginEntry {
            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            symbol: "static_vtable::entry".to_string(),
        });
    }
    let manifest_cstr = unsafe { CStr::from_ptr(manifest_ptr) };
    let manifest_text = manifest_cstr
        .to_str()
        .map_err(|_| PluginError::InvalidPluginEntry {
            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            symbol: "static_vtable::entry".to_string(),
            details: "embedded manifest is not valid UTF-8".to_string(),
        })?;

    let embedded_manifest = crate::PluginManifest::from_toml_str(manifest_text)?;
    let declaration = embedded_manifest.to_declaration()?;

    // Validate against host capabilities (skip entry file existence check
    // since there is no file -- the plugin is compiled into the binary).
    let synthetic = RegisteredPlugin {
        declaration: declaration.clone(),
        ..registered_plugin.clone()
    };
    PluginRegistry::validate_static_plugin(&synthetic, host, available_capabilities)?;

    compare_manifest_and_embedded(registered_plugin, &declaration)?;

    Ok(LoadedPlugin {
        registered: registered_plugin.clone(),
        declaration,
        backend: PluginBackend::Static(vtable),
    })
}

fn load_native_declaration(
    library: &Library,
    registered_plugin: &RegisteredPlugin,
) -> Result<PluginDeclaration> {
    let symbol_name = match &registered_plugin.declaration.entrypoint {
        PluginEntrypoint::Native { symbol } => symbol.as_bytes(),
    };

    let descriptor_symbol: Symbol<'_, PluginEntryFn> = unsafe { library.get(symbol_name) }
        .map_err(|error| PluginError::NativeEntrySymbol {
            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            symbol: match &registered_plugin.declaration.entrypoint {
                PluginEntrypoint::Native { symbol } => symbol.clone(),
            },
            details: error.to_string(),
        })?;

    let descriptor_ptr = unsafe { descriptor_symbol() };
    let symbol = match &registered_plugin.declaration.entrypoint {
        PluginEntrypoint::Native { symbol } => symbol.clone(),
    };
    if descriptor_ptr.is_null() {
        return Err(PluginError::NullPluginEntry {
            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            symbol,
        });
    }

    let manifest_text = unsafe { CStr::from_ptr(descriptor_ptr) }
        .to_str()
        .map_err(|_| PluginError::InvalidPluginEntryUtf8 {
            plugin_id: registered_plugin.declaration.id.as_str().to_string(),
            symbol: symbol.clone(),
        })?;

    let embedded_manifest =
        crate::PluginManifest::from_toml_str(manifest_text).map_err(|error| {
            PluginError::InvalidPluginEntry {
                plugin_id: registered_plugin.declaration.id.as_str().to_string(),
                symbol: symbol.clone(),
                details: error.to_string(),
            }
        })?;

    embedded_manifest.to_declaration()
}

fn compare_manifest_and_embedded(
    registered_plugin: &RegisteredPlugin,
    declaration: &PluginDeclaration,
) -> Result<()> {
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "id",
        registered_plugin.declaration.id.as_str(),
        declaration.id.as_str(),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "display_name",
        &registered_plugin.declaration.display_name,
        &declaration.display_name,
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "plugin_version",
        &registered_plugin.declaration.plugin_version,
        &declaration.plugin_version,
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "plugin_api",
        &registered_plugin.declaration.plugin_api.to_string(),
        &declaration.plugin_api.to_string(),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "native_abi",
        &registered_plugin.declaration.native_abi.to_string(),
        &declaration.native_abi.to_string(),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "provider_priority",
        &registered_plugin.declaration.provider_priority.to_string(),
        &declaration.provider_priority.to_string(),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "required_capabilities",
        &format!("{:?}", registered_plugin.declaration.required_capabilities),
        &format!("{:?}", declaration.required_capabilities),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "provided_capabilities",
        &format!("{:?}", registered_plugin.declaration.provided_capabilities),
        &format!("{:?}", declaration.provided_capabilities),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "provided_features",
        &format!("{:?}", registered_plugin.declaration.provided_features),
        &format!("{:?}", declaration.provided_features),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "services",
        &serde_json::to_string(&registered_plugin.declaration.services)
            .expect("plugin services should serialize"),
        &serde_json::to_string(&declaration.services).expect("plugin services should serialize"),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "commands",
        &serde_json::to_string(&registered_plugin.declaration.commands)
            .expect("plugin commands should serialize"),
        &serde_json::to_string(&declaration.commands).expect("plugin commands should serialize"),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "event_subscriptions",
        &serde_json::to_string(&registered_plugin.declaration.event_subscriptions)
            .expect("plugin event subscriptions should serialize"),
        &serde_json::to_string(&declaration.event_subscriptions)
            .expect("plugin event subscriptions should serialize"),
    )?;
    ensure_match(
        registered_plugin.declaration.id.as_str(),
        "dependencies",
        &serde_json::to_string(&registered_plugin.declaration.dependencies)
            .expect("plugin dependencies should serialize"),
        &serde_json::to_string(&declaration.dependencies)
            .expect("plugin dependencies should serialize"),
    )?;

    Ok(())
}

fn ensure_match(
    plugin_id: &str,
    field: &'static str,
    manifest_value: &str,
    embedded_value: &str,
) -> Result<()> {
    if manifest_value == embedded_value {
        Ok(())
    } else {
        Err(PluginError::ManifestMismatch {
            plugin_id: plugin_id.to_string(),
            field,
            manifest_value: manifest_value.to_string(),
            embedded_value: embedded_value.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{LoadedPlugin, PluginBackend};
    use crate::{
        ApiVersion, DEFAULT_NATIVE_ENTRY_SYMBOL, HostMetadata, NativeLifecycleContext,
        NativeServiceContext, PluginEntrypoint, PluginEvent, PluginEventKind,
        PluginEventSubscription, PluginManifest, PluginRegistry, ServiceCaller,
        ServiceEnvelopeKind, ServiceResponse, decode_service_envelope, decode_service_message,
        encode_service_envelope, encode_service_message,
    };
    use libloading::Library;
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::c_char;
    use std::ptr;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    static KERNEL_REQUESTS: Mutex<Vec<bmux_ipc::Request>> = Mutex::new(Vec::new());
    static OMIT_CURRENT_CLIENT_FROM_LIST: AtomicBool = AtomicBool::new(false);

    const TEST_MANIFEST_TEXT: &str = concat!(
        "id = \"test.plugin\"\n",
        "name = \"Test Plugin\"\n",
        "version = \"0.1.0\"\n",
        "entry = \"unused.dylib\"\n",
        "required_capabilities = [\"bmux.commands\"]\n\n",
        "[[commands]]\n",
        "name = \"hello\"\n",
        "summary = \"hello\"\n",
        "execution = \"provider_exec\"\n",
        "\0"
    );

    #[unsafe(no_mangle)]
    extern "C" fn bmux_plugin_entry_v1() -> *const c_char {
        TEST_MANIFEST_TEXT.as_ptr().cast()
    }

    #[unsafe(no_mangle)]
    extern "C" fn bmux_plugin_invoke_service_v1(
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
    ) -> i32 {
        let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
        let (request_id, context) =
            decode_service_envelope::<NativeServiceContext>(input, ServiceEnvelopeKind::Request)
                .expect("service request should decode");
        let response = ServiceResponse::ok(context.request.payload);
        let encoded = encode_service_envelope(request_id, ServiceEnvelopeKind::Response, &response)
            .expect("service response should encode");
        unsafe {
            *output_len = encoded.len();
        }
        if output_ptr.is_null() || encoded.len() > output_capacity {
            return 4;
        }
        unsafe {
            ptr::copy_nonoverlapping(encoded.as_ptr(), output_ptr, encoded.len());
        }
        0
    }

    unsafe extern "C" fn test_host_kernel_bridge(
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
    ) -> i32 {
        let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
        let bridge_request: super::HostKernelBridgeRequest = match decode_service_message(input) {
            Ok(request) => request,
            Err(_) => return 1,
        };
        let kernel_request: bmux_ipc::Request = match bmux_ipc::decode(&bridge_request.payload) {
            Ok(request) => request,
            Err(_) => return 1,
        };

        KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed")
            .push(kernel_request.clone());

        let kernel_response = match kernel_request {
            bmux_ipc::Request::NewSession { name: Some(name) } if name == "deny" => {
                bmux_ipc::Response::Err(bmux_ipc::ErrorResponse {
                    code: bmux_ipc::ErrorCode::InvalidRequest,
                    message: "session policy denied for this operation".to_string(),
                })
            }
            bmux_ipc::Request::NewSession { .. } => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::SessionCreated {
                    id: uuid::Uuid::new_v4(),
                    name: Some("created".to_string()),
                })
            }
            bmux_ipc::Request::KillSession { .. } => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::SessionKilled {
                    id: uuid::Uuid::new_v4(),
                })
            }
            bmux_ipc::Request::ListSessions => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::SessionList {
                    sessions: vec![bmux_ipc::SessionSummary {
                        id: uuid::Uuid::new_v4(),
                        name: Some("alpha".to_string()),
                        client_count: 1,
                    }],
                })
            }
            bmux_ipc::Request::WhoAmI => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ClientIdentity {
                    id: uuid::Uuid::from_u128(0x11111111_1111_1111_1111_111111111111),
                })
            }
            bmux_ipc::Request::ListClients => {
                let clients = if OMIT_CURRENT_CLIENT_FROM_LIST.load(Ordering::SeqCst) {
                    vec![bmux_ipc::ClientSummary {
                        id: uuid::Uuid::from_u128(0xaaaaaaaa_aaaa_aaaa_aaaa_aaaaaaaaaaaa),
                        selected_context_id: None,
                        selected_session_id: None,
                        following_client_id: None,
                        following_global: false,
                    }]
                } else {
                    vec![bmux_ipc::ClientSummary {
                        id: uuid::Uuid::from_u128(0x11111111_1111_1111_1111_111111111111),
                        selected_context_id: None,
                        selected_session_id: Some(uuid::Uuid::from_u128(
                            0x22222222_2222_2222_2222_222222222222,
                        )),
                        following_client_id: None,
                        following_global: false,
                    }]
                };
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ClientList { clients })
            }
            bmux_ipc::Request::ListContexts => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ContextList {
                    contexts: vec![bmux_ipc::ContextSummary {
                        id: uuid::Uuid::from_u128(0x33333333_3333_3333_3333_333333333333),
                        name: Some("ctx-alpha".to_string()),
                        attributes: BTreeMap::from([(
                            "core.kind".to_string(),
                            "workspace".to_string(),
                        )]),
                    }],
                })
            }
            bmux_ipc::Request::CurrentContext => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::CurrentContext {
                    context: Some(bmux_ipc::ContextSummary {
                        id: uuid::Uuid::from_u128(0x33333333_3333_3333_3333_333333333333),
                        name: Some("ctx-alpha".to_string()),
                        attributes: BTreeMap::new(),
                    }),
                })
            }
            bmux_ipc::Request::CreateContext { name, attributes } => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ContextCreated {
                    context: bmux_ipc::ContextSummary {
                        id: uuid::Uuid::new_v4(),
                        name,
                        attributes,
                    },
                })
            }
            bmux_ipc::Request::SelectContext { selector } => {
                let id = match selector {
                    bmux_ipc::ContextSelector::ById(id) => id,
                    bmux_ipc::ContextSelector::ByName(_) => uuid::Uuid::new_v4(),
                };
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ContextSelected {
                    context: bmux_ipc::ContextSummary {
                        id,
                        name: Some("ctx-selected".to_string()),
                        attributes: BTreeMap::new(),
                    },
                })
            }
            bmux_ipc::Request::CloseContext { selector, .. } => {
                let id = match selector {
                    bmux_ipc::ContextSelector::ById(id) => id,
                    bmux_ipc::ContextSelector::ByName(_) => uuid::Uuid::new_v4(),
                };
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::ContextClosed { id })
            }
            bmux_ipc::Request::Attach { selector } => {
                let session_id = match selector {
                    bmux_ipc::SessionSelector::ById(id) => id,
                    bmux_ipc::SessionSelector::ByName(_) => uuid::Uuid::new_v4(),
                };
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::Attached {
                    grant: bmux_ipc::AttachGrant {
                        context_id: None,
                        session_id,
                        attach_token: uuid::Uuid::new_v4(),
                        expires_at_epoch_ms: 42,
                    },
                })
            }
            bmux_ipc::Request::ListPanes { .. } => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::PaneList {
                    panes: vec![bmux_ipc::PaneSummary {
                        id: uuid::Uuid::new_v4(),
                        index: 1,
                        name: Some("pane-1".to_string()),
                        focused: true,
                    }],
                })
            }
            bmux_ipc::Request::SplitPane { .. } => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::PaneSplit {
                    id: uuid::Uuid::new_v4(),
                    session_id: uuid::Uuid::new_v4(),
                })
            }
            bmux_ipc::Request::FocusPane { .. } => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::PaneFocused {
                    id: uuid::Uuid::new_v4(),
                    session_id: uuid::Uuid::new_v4(),
                })
            }
            bmux_ipc::Request::ResizePane { .. } => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::PaneResized {
                    session_id: uuid::Uuid::new_v4(),
                })
            }
            bmux_ipc::Request::ClosePane { .. } => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::PaneClosed {
                    id: uuid::Uuid::new_v4(),
                    session_id: uuid::Uuid::new_v4(),
                    session_closed: false,
                })
            }
            _ => bmux_ipc::Response::Err(bmux_ipc::ErrorResponse {
                code: bmux_ipc::ErrorCode::InvalidRequest,
                message: "unsupported kernel request in test bridge".to_string(),
            }),
        };
        let encoded_kernel_response = match bmux_ipc::encode(&kernel_response) {
            Ok(payload) => payload,
            Err(_) => return 1,
        };
        let output_message = match encode_service_message(&super::HostKernelBridgeResponse {
            payload: encoded_kernel_response,
        }) {
            Ok(message) => message,
            Err(_) => return 1,
        };

        let required_len = output_message.len();
        if required_len > output_capacity {
            unsafe {
                *output_len = required_len;
            }
            return 4;
        }

        unsafe {
            ptr::copy_nonoverlapping(output_message.as_ptr(), output_ptr, required_len);
            *output_len = required_len;
        }
        0
    }

    #[test]
    fn loaded_plugin_reports_declared_commands() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "test.plugin"
name = "Test Plugin"
version = "0.1.0"
entry = "unused.dylib"
required_capabilities = ["bmux.commands"]

[[commands]]
name = "hello"
summary = "hello"
execution = "provider_exec"

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(std::path::Path::new("plugin.toml"), manifest)
            .expect("manifest should register");

        #[cfg(unix)]
        let library = Library::from(libloading::os::unix::Library::this());
        #[cfg(windows)]
        let library = Library::from(
            libloading::os::windows::Library::this().expect("current library should load"),
        );

        let loaded = LoadedPlugin {
            registered: registry
                .get("test.plugin")
                .expect("plugin should exist")
                .clone(),
            declaration: PluginManifest::from_toml_str(TEST_MANIFEST_TEXT.trim_end_matches('\0'))
                .expect("manifest should parse")
                .to_declaration()
                .expect("declaration should build"),
            backend: PluginBackend::Dynamic(library),
        };

        assert_eq!(loaded.commands().len(), 1);
        assert!(loaded.supports_command("hello"));
        assert!(loaded.run_command("missing", &[]).is_err());
    }

    #[test]
    fn lifecycle_context_serializes_settings_and_host() {
        let context = NativeLifecycleContext {
            plugin_id: "test.plugin".to_string(),
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            services: Vec::new(),
            available_capabilities: Vec::new(),
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: Some(toml::Value::String("enabled".to_string())),
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: None,
        };

        let json = serde_json::to_string(&context).expect("context should serialize");
        assert!(json.contains("test.plugin"));
        assert!(json.contains("bmux"));
        assert!(json.contains("enabled"));
    }

    #[test]
    fn command_context_call_service_rejects_missing_capability() {
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "hello".to_string(),
            arguments: Vec::new(),
            required_capabilities: Vec::new(),
            provided_capabilities: Vec::new(),
            services: vec![crate::RegisteredService {
                capability: crate::HostScope::new("bmux.permissions.read")
                    .expect("capability should parse"),
                kind: crate::ServiceKind::Query,
                interface_id: "permission-query/v1".to_string(),
                provider: crate::ProviderId::Plugin("bmux.permissions".to_string()),
            }],
            available_capabilities: vec!["bmux.permissions.read".to_string()],
            enabled_plugins: vec!["bmux.permissions".to_string()],
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: None,
        };

        let error = context
            .call_service_raw(
                "bmux.permissions.read",
                crate::ServiceKind::Query,
                "permission-query/v1",
                "list",
                Vec::new(),
            )
            .expect_err("missing capability should fail");
        assert!(error.to_string().contains("bmux.permissions.read"));
    }

    #[test]
    fn command_context_call_service_rejects_missing_registration() {
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "hello".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.permissions.read".to_string()],
            provided_capabilities: Vec::new(),
            services: Vec::new(),
            available_capabilities: vec!["bmux.permissions.read".to_string()],
            enabled_plugins: vec!["bmux.permissions".to_string()],
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: None,
        };

        let error = context
            .call_service_raw(
                "bmux.permissions.read",
                crate::ServiceKind::Query,
                "permission-query/v1",
                "list",
                Vec::new(),
            )
            .expect_err("missing service registration should fail");
        assert!(error.to_string().contains("call_service"));
    }

    #[test]
    fn command_context_calls_core_config_service() {
        let mut plugin_settings_map = BTreeMap::new();
        plugin_settings_map.insert(
            "caller.plugin".to_string(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "greeting".to_string(),
                toml::Value::String("hello".to_string()),
            )])),
        );
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "hello".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.config.read".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![crate::RegisteredService {
                capability: crate::HostScope::new("bmux.config.read")
                    .expect("capability should parse"),
                kind: crate::ServiceKind::Query,
                interface_id: "config-query/v1".to_string(),
                provider: crate::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.config.read".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map,
            host_kernel_bridge: None,
        };

        let response = context
            .call_service_raw(
                "bmux.config.read",
                crate::ServiceKind::Query,
                "config-query/v1",
                "plugin_settings",
                encode_service_message(&super::CorePluginSettingsRequest {
                    plugin_id: "caller.plugin".to_string(),
                })
                .expect("request should encode"),
            )
            .expect("core config service should succeed");
        let response: super::CorePluginSettingsResponse =
            decode_service_message(&response).expect("response should decode");
        assert_eq!(
            response.settings.get("greeting"),
            Some(&"\"hello\"".to_string())
        );
    }

    #[test]
    fn command_context_calls_core_storage_service() {
        let storage_root =
            std::env::temp_dir().join(format!("bmux-plugin-storage-test-{}", uuid::Uuid::new_v4()));
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "hello".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.storage".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![
                crate::RegisteredService {
                    capability: crate::HostScope::new("bmux.storage")
                        .expect("capability should parse"),
                    kind: crate::ServiceKind::Command,
                    interface_id: "storage-command/v1".to_string(),
                    provider: crate::ProviderId::Host,
                },
                crate::RegisteredService {
                    capability: crate::HostScope::new("bmux.storage")
                        .expect("capability should parse"),
                    kind: crate::ServiceKind::Query,
                    interface_id: "storage-query/v1".to_string(),
                    provider: crate::ProviderId::Host,
                },
            ],
            available_capabilities: vec!["bmux.storage".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: storage_root.to_string_lossy().to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: None,
        };

        context
            .call_service_raw(
                "bmux.storage",
                crate::ServiceKind::Command,
                "storage-command/v1",
                "set",
                encode_service_message(&super::CoreStorageSetRequest {
                    key: "theme".to_string(),
                    value: b"sunset".to_vec(),
                })
                .expect("set request should encode"),
            )
            .expect("core storage set should succeed");

        let bytes = context
            .call_service_raw(
                "bmux.storage",
                crate::ServiceKind::Query,
                "storage-query/v1",
                "get",
                encode_service_message(&super::CoreStorageGetRequest {
                    key: "theme".to_string(),
                })
                .expect("get request should encode"),
            )
            .expect("core storage get should succeed");
        let response: super::CoreStorageGetResponse =
            decode_service_message(&bytes).expect("get response should decode");
        assert_eq!(response.value, Some(b"sunset".to_vec()));

        let _ = std::fs::remove_dir_all(storage_root);
    }

    #[test]
    fn command_context_calls_core_logging_service() {
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "log".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.logs.write".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![crate::RegisteredService {
                capability: crate::HostScope::new("bmux.logs.write")
                    .expect("capability should parse"),
                kind: crate::ServiceKind::Command,
                interface_id: "logging-command/v1".to_string(),
                provider: crate::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.logs.write".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: None,
        };

        let response: () = context
            .call_service(
                "bmux.logs.write",
                crate::ServiceKind::Command,
                "logging-command/v1",
                "write",
                &crate::LogWriteRequest {
                    level: crate::LogWriteLevel::Info,
                    message: "hello from plugin".to_string(),
                    target: Some("plugin.test".to_string()),
                },
            )
            .expect("core logging service should succeed");
        assert_eq!(response, ());
    }

    #[test]
    fn command_context_calls_core_session_query_via_kernel_bridge() {
        KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed")
            .clear();

        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "list-sessions".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.sessions.read".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![crate::RegisteredService {
                capability: crate::HostScope::new("bmux.sessions.read")
                    .expect("capability should parse"),
                kind: crate::ServiceKind::Query,
                interface_id: "session-query/v1".to_string(),
                provider: crate::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.sessions.read".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: Some(super::HostKernelBridge::from_fn(test_host_kernel_bridge)),
        };

        let response: crate::SessionListResponse = context
            .call_service(
                "bmux.sessions.read",
                crate::ServiceKind::Query,
                "session-query/v1",
                "list",
                &(),
            )
            .expect("core session query should succeed");
        assert_eq!(response.sessions.len(), 1);

        let requests = KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed");
        assert!(matches!(
            requests.last(),
            Some(bmux_ipc::Request::ListSessions)
        ));
    }

    #[test]
    fn command_context_calls_core_pane_command_via_kernel_bridge() {
        KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed")
            .clear();

        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "split-pane".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.panes.write".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![crate::RegisteredService {
                capability: crate::HostScope::new("bmux.panes.write")
                    .expect("capability should parse"),
                kind: crate::ServiceKind::Command,
                interface_id: "pane-command/v1".to_string(),
                provider: crate::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.panes.write".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: Some(super::HostKernelBridge::from_fn(test_host_kernel_bridge)),
        };

        let _response: crate::PaneSplitResponse = context
            .call_service(
                "bmux.panes.write",
                crate::ServiceKind::Command,
                "pane-command/v1",
                "split",
                &crate::PaneSplitRequest {
                    session: None,
                    target: None,
                    direction: crate::PaneSplitDirection::Vertical,
                },
            )
            .expect("core pane command should succeed");

        let requests = KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed");
        assert!(matches!(
            requests.last(),
            Some(bmux_ipc::Request::SplitPane { .. })
        ));
    }

    #[test]
    fn command_context_calls_core_session_command_via_kernel_bridge() {
        KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed")
            .clear();

        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "new-session".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.sessions.write".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![crate::RegisteredService {
                capability: crate::HostScope::new("bmux.sessions.write")
                    .expect("capability should parse"),
                kind: crate::ServiceKind::Command,
                interface_id: "session-command/v1".to_string(),
                provider: crate::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.sessions.write".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: Some(super::HostKernelBridge::from_fn(test_host_kernel_bridge)),
        };

        let _response: crate::SessionCreateResponse = context
            .call_service(
                "bmux.sessions.write",
                crate::ServiceKind::Command,
                "session-command/v1",
                "new",
                &crate::SessionCreateRequest {
                    name: Some("created".to_string()),
                },
            )
            .expect("core session command should succeed");

        let requests = KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed");
        assert!(matches!(
            requests.last(),
            Some(bmux_ipc::Request::NewSession { .. })
        ));
    }

    #[test]
    fn command_context_calls_core_client_query_via_kernel_bridge() {
        KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed")
            .clear();

        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "current-client".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.clients.read".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![crate::RegisteredService {
                capability: crate::HostScope::new("bmux.clients.read")
                    .expect("capability should parse"),
                kind: crate::ServiceKind::Query,
                interface_id: "client-query/v1".to_string(),
                provider: crate::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.clients.read".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: Some(super::HostKernelBridge::from_fn(test_host_kernel_bridge)),
        };

        let response: crate::CurrentClientResponse = context
            .call_service(
                "bmux.clients.read",
                crate::ServiceKind::Query,
                "client-query/v1",
                "current",
                &(),
            )
            .expect("core client query should succeed");
        assert_eq!(
            response.id,
            uuid::Uuid::from_u128(0x11111111_1111_1111_1111_111111111111)
        );
        assert_eq!(
            response.selected_session_id,
            Some(uuid::Uuid::from_u128(
                0x22222222_2222_2222_2222_222222222222
            ))
        );

        let requests = KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed");
        assert!(
            requests
                .iter()
                .any(|request| matches!(request, bmux_ipc::Request::WhoAmI))
        );
        assert!(
            requests
                .iter()
                .any(|request| matches!(request, bmux_ipc::Request::ListClients))
        );
    }

    #[test]
    fn command_context_current_client_tolerates_missing_list_clients_entry() {
        KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed")
            .clear();
        OMIT_CURRENT_CLIENT_FROM_LIST.store(true, Ordering::SeqCst);

        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "current-client".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.clients.read".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![crate::RegisteredService {
                capability: crate::HostScope::new("bmux.clients.read")
                    .expect("capability should parse"),
                kind: crate::ServiceKind::Query,
                interface_id: "client-query/v1".to_string(),
                provider: crate::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.clients.read".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: Some(super::HostKernelBridge::from_fn(test_host_kernel_bridge)),
        };

        let response: crate::CurrentClientResponse = context
            .call_service(
                "bmux.clients.read",
                crate::ServiceKind::Query,
                "client-query/v1",
                "current",
                &(),
            )
            .expect("core client query should succeed when list-clients omits current client");
        assert_eq!(
            response.id,
            uuid::Uuid::from_u128(0x11111111_1111_1111_1111_111111111111)
        );
        assert_eq!(response.selected_session_id, None);
        assert_eq!(response.following_client_id, None);
        assert!(!response.following_global);

        OMIT_CURRENT_CLIENT_FROM_LIST.store(false, Ordering::SeqCst);
    }

    #[test]
    fn command_context_calls_core_session_select_via_kernel_bridge() {
        KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed")
            .clear();

        let target_session_id = uuid::Uuid::new_v4();
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "select-session".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.sessions.write".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![crate::RegisteredService {
                capability: crate::HostScope::new("bmux.sessions.write")
                    .expect("capability should parse"),
                kind: crate::ServiceKind::Command,
                interface_id: "session-command/v1".to_string(),
                provider: crate::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.sessions.write".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: Some(super::HostKernelBridge::from_fn(test_host_kernel_bridge)),
        };

        let response: crate::SessionSelectResponse = context
            .call_service(
                "bmux.sessions.write",
                crate::ServiceKind::Command,
                "session-command/v1",
                "select",
                &crate::SessionSelectRequest {
                    selector: crate::SessionSelector::ById(target_session_id),
                },
            )
            .expect("core session select should succeed");
        assert_eq!(response.session_id, target_session_id);
        assert_eq!(response.expires_at_epoch_ms, 42);

        let requests = KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed");
        assert!(requests.iter().any(|request| {
            matches!(
                request,
                bmux_ipc::Request::Attach {
                    selector: bmux_ipc::SessionSelector::ById(id)
                } if *id == target_session_id
            )
        }));
    }

    #[test]
    fn command_context_surfaces_kernel_error_for_session_command() {
        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "new-session".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.sessions.write".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![crate::RegisteredService {
                capability: crate::HostScope::new("bmux.sessions.write")
                    .expect("capability should parse"),
                kind: crate::ServiceKind::Command,
                interface_id: "session-command/v1".to_string(),
                provider: crate::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.sessions.write".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: Some(super::HostKernelBridge::from_fn(test_host_kernel_bridge)),
        };

        let error = context
            .call_service::<crate::SessionCreateRequest, crate::SessionCreateResponse>(
                "bmux.sessions.write",
                crate::ServiceKind::Command,
                "session-command/v1",
                "new",
                &crate::SessionCreateRequest {
                    name: Some("deny".to_string()),
                },
            )
            .expect_err("kernel denial should propagate as service error");

        assert!(
            error
                .to_string()
                .contains("session policy denied for this operation")
        );
    }

    #[test]
    fn command_context_calls_core_pane_query_via_kernel_bridge() {
        KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed")
            .clear();

        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "list-panes".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.panes.read".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![crate::RegisteredService {
                capability: crate::HostScope::new("bmux.panes.read")
                    .expect("capability should parse"),
                kind: crate::ServiceKind::Query,
                interface_id: "pane-query/v1".to_string(),
                provider: crate::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.panes.read".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: Some(super::HostKernelBridge::from_fn(test_host_kernel_bridge)),
        };

        let response: crate::PaneListResponse = context
            .call_service(
                "bmux.panes.read",
                crate::ServiceKind::Query,
                "pane-query/v1",
                "list",
                &crate::PaneListRequest { session: None },
            )
            .expect("core pane query should succeed");
        assert_eq!(response.panes.len(), 1);

        let requests = KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed");
        assert!(matches!(
            requests.last(),
            Some(bmux_ipc::Request::ListPanes { .. })
        ));
    }

    #[test]
    fn command_context_calls_focus_resize_close_via_kernel_bridge() {
        KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed")
            .clear();

        let context = super::NativeCommandContext {
            plugin_id: "caller.plugin".to_string(),
            command: "pane-ops".to_string(),
            arguments: Vec::new(),
            required_capabilities: vec!["bmux.panes.write".to_string()],
            provided_capabilities: Vec::new(),
            services: vec![crate::RegisteredService {
                capability: crate::HostScope::new("bmux.panes.write")
                    .expect("capability should parse"),
                kind: crate::ServiceKind::Command,
                interface_id: "pane-command/v1".to_string(),
                provider: crate::ProviderId::Host,
            }],
            available_capabilities: vec!["bmux.panes.write".to_string()],
            enabled_plugins: Vec::new(),
            plugin_search_roots: Vec::new(),
            registered_plugins: Vec::new(),
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: BTreeMap::new(),
            host_kernel_bridge: Some(super::HostKernelBridge::from_fn(test_host_kernel_bridge)),
        };

        let _focused: crate::PaneFocusResponse = context
            .call_service(
                "bmux.panes.write",
                crate::ServiceKind::Command,
                "pane-command/v1",
                "focus",
                &crate::PaneFocusRequest {
                    session: None,
                    target: Some(crate::PaneSelector::Active),
                    direction: Some(crate::PaneFocusDirection::Next),
                },
            )
            .expect("focus command should succeed");

        let _resized: crate::PaneResizeResponse = context
            .call_service(
                "bmux.panes.write",
                crate::ServiceKind::Command,
                "pane-command/v1",
                "resize",
                &crate::PaneResizeRequest {
                    session: None,
                    target: Some(crate::PaneSelector::Active),
                    delta: 1,
                },
            )
            .expect("resize command should succeed");

        let _closed: crate::PaneCloseResponse = context
            .call_service(
                "bmux.panes.write",
                crate::ServiceKind::Command,
                "pane-command/v1",
                "close",
                &crate::PaneCloseRequest {
                    session: None,
                    target: Some(crate::PaneSelector::Active),
                },
            )
            .expect("close command should succeed");

        let requests = KERNEL_REQUESTS
            .lock()
            .expect("kernel request log lock should succeed");
        assert!(
            requests
                .iter()
                .any(|request| matches!(request, bmux_ipc::Request::FocusPane { .. }))
        );
        assert!(
            requests
                .iter()
                .any(|request| matches!(request, bmux_ipc::Request::ResizePane { .. }))
        );
        assert!(
            requests
                .iter()
                .any(|request| matches!(request, bmux_ipc::Request::ClosePane { .. }))
        );
    }

    #[test]
    fn native_service_context_roundtrips_through_service_envelope() {
        let context = NativeServiceContext {
            plugin_id: "bmux.permissions".to_string(),
            request: crate::ServiceRequest {
                caller_plugin_id: "example.native".to_string(),
                service: crate::RegisteredService {
                    capability: crate::HostScope::new("bmux.permissions.read")
                        .expect("capability should parse"),
                    kind: crate::ServiceKind::Query,
                    interface_id: "permission-query/v1".to_string(),
                    provider: crate::ProviderId::Plugin("bmux.permissions".to_string()),
                },
                operation: "list".to_string(),
                payload: vec![1, 2, 3],
            },
            required_capabilities: vec!["bmux.permissions.read".to_string()],
            provided_capabilities: vec!["bmux.permissions.read".to_string()],
            services: Vec::new(),
            available_capabilities: vec!["bmux.permissions.read".to_string()],
            enabled_plugins: vec!["bmux.permissions".to_string()],
            plugin_search_roots: vec!["/plugins".to_string()],
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: crate::HostConnectionInfo {
                config_dir: "/config".to_string(),
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: BTreeMap::from([("mode".to_string(), "\"service\"".to_string())]),
            plugin_settings_map: BTreeMap::from([(
                "example.native".to_string(),
                BTreeMap::from([("mode".to_string(), "\"service\"".to_string())]),
            )]),
            host_kernel_bridge: None,
        };

        let bytes = encode_service_envelope(7, ServiceEnvelopeKind::Request, &context)
            .expect("context should encode");
        let (request_id, decoded): (u64, NativeServiceContext) =
            decode_service_envelope(&bytes, ServiceEnvelopeKind::Request)
                .expect("context should decode");
        assert_eq!(request_id, 7);
        assert_eq!(decoded, context);
    }

    #[test]
    fn loaded_plugin_filters_events_by_subscription() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "test.plugin"
name = "Test Plugin"
version = "0.1.0"
entry = "unused.dylib"

[[event_subscriptions]]
kinds = ["system"]
names = ["server_started"]

[plugin_api]
minimum = "1.0"

[native_abi]
minimum = "1.0"
"#,
        )
        .expect("manifest should parse");
        let mut registry = PluginRegistry::new();
        registry
            .register_manifest(std::path::Path::new("plugin.toml"), manifest)
            .expect("manifest should register");

        #[cfg(unix)]
        let library = Library::from(libloading::os::unix::Library::this());
        #[cfg(windows)]
        let library = Library::from(
            libloading::os::windows::Library::this().expect("current library should load"),
        );

        let loaded = LoadedPlugin {
            registered: registry
                .get("test.plugin")
                .expect("plugin should exist")
                .clone(),
            declaration: crate::PluginDeclaration {
                id: crate::PluginId::new("test.plugin").expect("plugin id should parse"),
                display_name: "Test Plugin".to_string(),
                plugin_version: "0.1.0".to_string(),
                plugin_api: crate::VersionRange::at_least(ApiVersion::new(1, 0)),
                native_abi: crate::VersionRange::at_least(ApiVersion::new(1, 0)),
                entrypoint: PluginEntrypoint::Native {
                    symbol: DEFAULT_NATIVE_ENTRY_SYMBOL.to_string(),
                },
                description: None,
                homepage: None,
                provider_priority: 0,
                required_capabilities: BTreeSet::new(),
                provided_capabilities: BTreeSet::new(),
                provided_features: BTreeSet::new(),
                services: Vec::new(),
                commands: Vec::new(),
                event_subscriptions: vec![PluginEventSubscription {
                    kinds: BTreeSet::from([PluginEventKind::System]),
                    names: BTreeSet::from(["server_started".to_string()]),
                }],
                dependencies: Vec::new(),
                lifecycle: crate::PluginLifecycle::default(),
            },
            backend: PluginBackend::Dynamic(library),
        };

        assert!(loaded.receives_event(&PluginEvent {
            kind: PluginEventKind::System,
            name: "server_started".to_string(),
            payload: serde_json::Value::Null,
        }));
        assert!(!loaded.receives_event(&PluginEvent {
            kind: PluginEventKind::System,
            name: "server_stopping".to_string(),
            payload: serde_json::Value::Null,
        }));
    }

    #[test]
    fn production_loader_code_does_not_hardcode_domain_service_interfaces() {
        let source = include_str!("loader.rs")
            .split("\n#[cfg(test)]")
            .next()
            .unwrap_or_default();
        assert!(!source.contains("permission-query/v1"));
        assert!(!source.contains("permission-command/v1"));
        assert!(!source.contains("window-query/v1"));
        assert!(!source.contains("window-command/v1"));
    }
}
