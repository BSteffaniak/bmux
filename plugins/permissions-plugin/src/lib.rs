#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin::{
    CommandExecutionKind, HostRuntimeApi, HostScope, NativeCommandContext, NativeDescriptor,
    NativeServiceContext, PluginCommand, PluginCommandArgument, PluginCommandArgumentKind,
    PluginService, RustPlugin, ServiceKind, ServiceResponse, SessionSelector, StorageGetRequest,
    StorageSetRequest, decode_service_message, encode_service_message,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Default)]
struct PermissionsPlugin;

impl RustPlugin for PermissionsPlugin {
    fn descriptor(&self) -> NativeDescriptor {
        NativeDescriptor::builder("bmux.permissions", "bmux Permissions")
            .plugin_version(env!("CARGO_PKG_VERSION"))
            .description("Shipped bmux permissions command plugin")
            .require_capability("bmux.commands")
            .expect("capability should parse")
            .require_capability("bmux.sessions.read")
            .expect("capability should parse")
            .require_capability("bmux.clients.read")
            .expect("capability should parse")
            .require_capability("bmux.storage")
            .expect("capability should parse")
            .provide_capability("bmux.permissions.read")
            .expect("capability should parse")
            .provide_capability("bmux.permissions.write")
            .expect("capability should parse")
            .provide_capability("bmux.sessions.policy")
            .expect("capability should parse")
            .provide_feature("bmux.permissions")
            .expect("feature should parse")
            .service(PluginService {
                capability: HostScope::new("bmux.permissions.read")
                    .expect("host scope should parse"),
                kind: ServiceKind::Query,
                interface_id: "permission-query/v1".to_string(),
            })
            .service(PluginService {
                capability: HostScope::new("bmux.permissions.write")
                    .expect("host scope should parse"),
                kind: ServiceKind::Command,
                interface_id: "permission-command/v1".to_string(),
            })
            .service(PluginService {
                capability: HostScope::new("bmux.sessions.policy")
                    .expect("host scope should parse"),
                kind: ServiceKind::Query,
                interface_id: "session-policy-query/v1".to_string(),
            })
            .command(
                PluginCommand::new("permissions", "Permissions provider status")
                    .path(["permissions"])
                    .alias(["session", "permissions"])
                    .argument(
                        PluginCommandArgument::option("session", PluginCommandArgumentKind::String)
                            .required(true)
                            .short('s'),
                    )
                    .argument(PluginCommandArgument::flag("json").short('j'))
                    .execution(CommandExecutionKind::ProviderExec)
                    .expose_in_cli(true),
            )
            .command(
                PluginCommand::new(
                    "permissions-current",
                    "Permissions for the currently active session",
                )
                .path(["permissions-current"])
                .alias(["session", "permissions-current"])
                .argument(PluginCommandArgument::flag("json").short('j'))
                .execution(CommandExecutionKind::ProviderExec)
                .expose_in_cli(true),
            )
            .command(
                PluginCommand::new("grant", "Grant command handled by permissions provider")
                    .path(["grant"])
                    .alias(["session", "grant"])
                    .argument(
                        PluginCommandArgument::option("session", PluginCommandArgumentKind::String)
                            .required(true)
                            .short('s'),
                    )
                    .argument(
                        PluginCommandArgument::option("client", PluginCommandArgumentKind::String)
                            .required(true)
                            .short('c'),
                    )
                    .argument(
                        PluginCommandArgument::option("role", PluginCommandArgumentKind::Choice)
                            .required(true)
                            .short('r')
                            .choice_values(["owner", "writer", "observer"]),
                    )
                    .execution(CommandExecutionKind::ProviderExec)
                    .expose_in_cli(true),
            )
            .command(
                PluginCommand::new("revoke", "Revoke command handled by permissions provider")
                    .path(["revoke"])
                    .alias(["session", "revoke"])
                    .argument(
                        PluginCommandArgument::option("session", PluginCommandArgumentKind::String)
                            .required(true)
                            .short('s'),
                    )
                    .argument(
                        PluginCommandArgument::option("client", PluginCommandArgumentKind::String)
                            .required(true)
                            .short('c'),
                    )
                    .execution(CommandExecutionKind::ProviderExec)
                    .expose_in_cli(true),
            )
            .build()
            .expect("descriptor should validate")
    }

    fn run_command(&mut self, context: NativeCommandContext) -> i32 {
        match handle_command(&context) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{error}");
                1
            }
        }
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match (
            context.request.service.interface_id.as_str(),
            context.request.operation.as_str(),
        ) {
            ("permission-query/v1", "list") => {
                let request = match decode_service_message::<ListPermissionsRequest>(
                    &context.request.payload,
                ) {
                    Ok(request) => request,
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error.to_string());
                    }
                };
                let entries = match list_entries(&context, &request.session) {
                    Ok(entries) => entries,
                    Err(error) => {
                        return ServiceResponse::error("list_failed", error.to_string());
                    }
                };
                let payload = match encode_service_message(&ListPermissionsResponse { entries }) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            ("permission-command/v1", "grant") => {
                let request = match decode_service_message::<GrantRequest>(&context.request.payload)
                {
                    Ok(request) => request,
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error.to_string());
                    }
                };
                if let Err(error) = grant_entry(&context, request) {
                    return ServiceResponse::error("grant_failed", error.to_string());
                }
                let payload = match encode_service_message(&CommandAckResponse { ok: true }) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            ("permission-command/v1", "revoke") => {
                let request =
                    match decode_service_message::<RevokeRequest>(&context.request.payload) {
                        Ok(request) => request,
                        Err(error) => {
                            return ServiceResponse::error("invalid_request", error.to_string());
                        }
                    };
                if let Err(error) = revoke_entry(&context, request) {
                    return ServiceResponse::error("revoke_failed", error.to_string());
                }
                let payload = match encode_service_message(&CommandAckResponse { ok: true }) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            ("session-policy-query/v1", "check") => {
                let request = match decode_service_message::<SessionPolicyCheckRequest>(
                    &context.request.payload,
                ) {
                    Ok(request) => request,
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error.to_string());
                    }
                };
                let decision = match evaluate_policy(&context, &request) {
                    Ok(decision) => decision,
                    Err(error) => {
                        return ServiceResponse::error("policy_failed", error.to_string());
                    }
                };
                let payload = match encode_service_message(&decision) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            _ => ServiceResponse::error(
                "unsupported_service_operation",
                format!(
                    "unsupported permissions service invocation '{}:{}'",
                    context.request.service.interface_id, context.request.operation,
                ),
            ),
        }
    }
}

fn handle_command(context: &NativeCommandContext) -> Result<(), String> {
    match context.command.as_str() {
        "permissions" => {
            let session = required_option_value(&context.arguments, "session")?;
            let as_json = has_flag(&context.arguments, "json");
            let entries = list_entries(context, &session)?;
            if as_json {
                let output = serde_json::to_string_pretty(&ListPermissionsResponse { entries })
                    .map_err(|error| error.to_string())?;
                println!("{output}");
            } else if entries.is_empty() {
                println!("no explicit permissions for session {session}");
            } else {
                for entry in entries {
                    println!("{}\t{}", entry.client_id, entry.role);
                }
            }
            Ok(())
        }
        "permissions-current" => {
            let session = resolve_current_session(context)?;
            let as_json = has_flag(&context.arguments, "json");
            let entries = list_entries(context, &session)?;
            if as_json {
                let output = serde_json::to_string_pretty(&ListPermissionsResponse { entries })
                    .map_err(|error| error.to_string())?;
                println!("{output}");
            } else if entries.is_empty() {
                println!("no explicit permissions for session {session}");
            } else {
                for entry in entries {
                    println!("{}\t{}", entry.client_id, entry.role);
                }
            }
            Ok(())
        }
        "grant" => {
            let request = GrantRequest {
                session: required_option_value(&context.arguments, "session")?,
                client: required_option_value(&context.arguments, "client")?,
                role: required_option_value(&context.arguments, "role")?,
            };
            grant_entry(context, request)?;
            println!("granted permission");
            Ok(())
        }
        "revoke" => {
            let request = RevokeRequest {
                session: required_option_value(&context.arguments, "session")?,
                client: required_option_value(&context.arguments, "client")?,
            };
            revoke_entry(context, request)?;
            println!("revoked permission");
            Ok(())
        }
        _ => Err(format!("unsupported command '{}'", context.command)),
    }
}

fn resolve_current_session(caller: &impl HostRuntimeApi) -> Result<String, String> {
    let current_client = caller.current_client().map_err(|error| error.to_string())?;
    let sessions = caller
        .session_list()
        .map_err(|error| error.to_string())?
        .sessions;
    let preferred = current_client.selected_session_id.and_then(|selected_id| {
        sessions
            .iter()
            .find(|session| session.id == selected_id)
            .cloned()
    });
    let session = preferred
        .or_else(|| sessions.into_iter().next())
        .ok_or_else(|| "no active session available".to_string())?;
    Ok(session.name.unwrap_or_else(|| session.id.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredPermissions {
    by_session_id: BTreeMap<Uuid, Vec<PermissionEntry>>,
}

impl StoredPermissions {
    fn with_default() -> Self {
        Self {
            by_session_id: BTreeMap::new(),
        }
    }
}

const PERMISSIONS_STORAGE_KEY: &str = "permissions-v1";

fn list_entries(
    caller: &impl HostRuntimeApi,
    session: &str,
) -> Result<Vec<PermissionEntry>, String> {
    let session_id = resolve_session_id(caller, session)?;
    let state = load_state(caller)?;
    Ok(state
        .by_session_id
        .get(&session_id)
        .cloned()
        .unwrap_or_default())
}

fn grant_entry(caller: &impl HostRuntimeApi, request: GrantRequest) -> Result<(), String> {
    validate_role(&request.role)?;
    let session_id = resolve_session_id(caller, &request.session)?;
    let mut state = load_state(caller)?;
    let entries = state.by_session_id.entry(session_id).or_default();
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.client_id == request.client)
    {
        entry.role = request.role;
    } else {
        entries.push(PermissionEntry {
            client_id: request.client,
            role: request.role,
        });
    }
    save_state(caller, &state)
}

fn revoke_entry(caller: &impl HostRuntimeApi, request: RevokeRequest) -> Result<(), String> {
    let session_id = resolve_session_id(caller, &request.session)?;
    let mut state = load_state(caller)?;
    if let Some(entries) = state.by_session_id.get_mut(&session_id) {
        entries.retain(|entry| entry.client_id != request.client);
    }
    save_state(caller, &state)
}

fn evaluate_policy(
    caller: &impl HostRuntimeApi,
    request: &SessionPolicyCheckRequest,
) -> Result<SessionPolicyCheckResponse, String> {
    let state = load_state(caller)?;
    let entries = state
        .by_session_id
        .get(&request.session_id)
        .cloned()
        .unwrap_or_default();
    let client_key = request.client_id.to_string();
    let entry = entries
        .into_iter()
        .find(|entry| entry.client_id == client_key);

    let decision = match entry.as_ref().map(|entry| entry.role.as_str()) {
        None => SessionPolicyCheckResponse {
            allowed: true,
            reason: None,
        },
        Some(role) => evaluate_role_action(role, request.action.as_str()),
    };
    Ok(decision)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PolicyActionKind {
    Admin,
    Mutation,
    Read,
    Unknown,
}

fn evaluate_role_action(role: &str, action: &str) -> SessionPolicyCheckResponse {
    let action_kind = classify_action(action);
    match (role, action_kind) {
        (
            "owner",
            PolicyActionKind::Admin | PolicyActionKind::Mutation | PolicyActionKind::Read,
        ) => SessionPolicyCheckResponse {
            allowed: true,
            reason: None,
        },
        ("writer", PolicyActionKind::Mutation | PolicyActionKind::Read) => {
            SessionPolicyCheckResponse {
                allowed: true,
                reason: None,
            }
        }
        ("observer", PolicyActionKind::Read) => SessionPolicyCheckResponse {
            allowed: true,
            reason: None,
        },
        ("writer" | "observer", PolicyActionKind::Admin)
        | ("observer", PolicyActionKind::Mutation) => SessionPolicyCheckResponse {
            allowed: false,
            reason: Some(format!(
                "session policy denied for action '{}' with role '{}'",
                action, role
            )),
        },
        (_, PolicyActionKind::Unknown) => SessionPolicyCheckResponse {
            allowed: false,
            reason: Some(format!("invalid session policy action '{}'", action)),
        },
        (_, _) => SessionPolicyCheckResponse {
            allowed: false,
            reason: Some(format!("invalid session policy role mapping '{}'", role)),
        },
    }
}

fn classify_action(action: &str) -> PolicyActionKind {
    let normalized = action.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "admin" | "session.kill" => PolicyActionKind::Admin,
        "mutation" | "attach.input" | "session.select" | "pane.split" | "pane.focus"
        | "pane.resize" | "pane.close" | "follow" | "unfollow" | "context.create"
        | "context.select" | "context.close" => PolicyActionKind::Mutation,
        "read" | "list" | "status" | "context.list" => PolicyActionKind::Read,
        _ => PolicyActionKind::Unknown,
    }
}

fn load_state(caller: &impl HostRuntimeApi) -> Result<StoredPermissions, String> {
    let response = caller
        .storage_get(&StorageGetRequest {
            key: PERMISSIONS_STORAGE_KEY.to_string(),
        })
        .map_err(|error| error.to_string())?;
    match response.value {
        Some(value) => decode_service_message(&value).map_err(|error| error.to_string()),
        None => Ok(StoredPermissions::with_default()),
    }
}

fn save_state(caller: &impl HostRuntimeApi, state: &StoredPermissions) -> Result<(), String> {
    let value = encode_service_message(state).map_err(|error| error.to_string())?;
    caller
        .storage_set(&StorageSetRequest {
            key: PERMISSIONS_STORAGE_KEY.to_string(),
            value,
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn resolve_session_id(caller: &impl HostRuntimeApi, session: &str) -> Result<Uuid, String> {
    let selector = if let Ok(id) = Uuid::parse_str(session) {
        SessionSelector::ById(id)
    } else if session.trim().is_empty() {
        return Err("session must not be empty".to_string());
    } else {
        SessionSelector::ByName(session.to_string())
    };
    let sessions = caller
        .session_list()
        .map_err(|error| error.to_string())?
        .sessions;
    sessions
        .into_iter()
        .find(|entry| match &selector {
            SessionSelector::ById(id) => entry.id == *id,
            SessionSelector::ByName(name) => entry.name.as_deref() == Some(name.as_str()),
        })
        .map(|entry| entry.id)
        .ok_or_else(|| format!("session '{}' not found", session))
}

fn validate_role(role: &str) -> Result<(), String> {
    if matches!(role, "owner" | "writer" | "observer") {
        Ok(())
    } else {
        Err(format!(
            "invalid role '{}'; expected one of: owner, writer, observer",
            role
        ))
    }
}

fn required_option_value(arguments: &[String], long_name: &str) -> Result<String, String> {
    option_value(arguments, long_name)
        .ok_or_else(|| format!("missing required --{long_name} option"))
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListPermissionsRequest {
    session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GrantRequest {
    session: String,
    client: String,
    role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RevokeRequest {
    session: String,
    client: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SessionPolicyCheckRequest {
    session_id: Uuid,
    client_id: Uuid,
    principal_id: Uuid,
    action: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SessionPolicyCheckResponse {
    allowed: bool,
    reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PermissionEntry {
    client_id: String,
    role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListPermissionsResponse {
    entries: Vec<PermissionEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CommandAckResponse {
    ok: bool,
}

bmux_plugin::export_plugin!(PermissionsPlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin::{
        ApiVersion, HostConnectionInfo, HostKernelBridge, HostMetadata, NativeServiceContext,
        ProviderId, RegisteredService, ServiceCaller, ServiceRequest, SessionListResponse,
        SessionSummary,
    };
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct MockHost {
        sessions: Vec<SessionSummary>,
        selected_session_id: Option<Uuid>,
        storage: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl MockHost {
        fn with_session(id: Uuid, name: &str) -> Self {
            Self {
                sessions: vec![SessionSummary {
                    id,
                    name: Some(name.to_string()),
                    client_count: 1,
                }],
                selected_session_id: Some(id),
                storage: Mutex::new(BTreeMap::new()),
            }
        }

        fn with_sessions(sessions: Vec<SessionSummary>) -> Self {
            Self {
                selected_session_id: sessions.first().map(|session| session.id),
                sessions,
                storage: Mutex::new(BTreeMap::new()),
            }
        }

        fn with_sessions_and_selected(
            sessions: Vec<SessionSummary>,
            selected_session_id: Option<Uuid>,
        ) -> Self {
            Self {
                sessions,
                selected_session_id,
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
        ) -> bmux_plugin::Result<Vec<u8>> {
            match (interface_id, operation) {
                ("session-query/v1", "list") => encode_service_message(&SessionListResponse {
                    sessions: self.sessions.clone(),
                })
                .map_err(Into::into),
                ("client-query/v1", "current") => {
                    encode_service_message(&bmux_plugin::CurrentClientResponse {
                        id: Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111),
                        selected_session_id: self.selected_session_id,
                        following_client_id: None,
                        following_global: false,
                    })
                    .map_err(Into::into)
                }
                ("storage-query/v1", "get") => {
                    let request: StorageGetRequest = decode_service_message(&payload)?;
                    let value = self
                        .storage
                        .lock()
                        .expect("storage lock should succeed")
                        .get(&request.key)
                        .cloned();
                    encode_service_message(&bmux_plugin::StorageGetResponse { value })
                        .map_err(Into::into)
                }
                ("storage-command/v1", "set") => {
                    let request: StorageSetRequest = decode_service_message(&payload)?;
                    self.storage
                        .lock()
                        .expect("storage lock should succeed")
                        .insert(request.key, request.value);
                    encode_service_message(&()).map_err(Into::into)
                }
                _ => Err(bmux_plugin::PluginError::UnsupportedHostOperation {
                    operation: "mock_service",
                }),
            }
        }
    }

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
        let request: BridgeRequest = match decode_service_message(input) {
            Ok(request) => request,
            Err(_) => return 1,
        };
        let kernel_request: bmux_ipc::Request = match bmux_ipc::decode(&request.payload) {
            Ok(request) => request,
            Err(_) => return 1,
        };

        let response = match kernel_request {
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
                        selected_session_id: Some(service_test_session_id()),
                        following_client_id: None,
                        following_global: false,
                    }],
                })
            }
            bmux_ipc::Request::ListSessions => {
                bmux_ipc::Response::Ok(bmux_ipc::ResponsePayload::SessionList {
                    sessions: vec![bmux_ipc::SessionSummary {
                        id: service_test_session_id(),
                        name: Some("alpha".to_string()),
                        client_count: 1,
                    }],
                })
            }
            _ => bmux_ipc::Response::Err(bmux_ipc::ErrorResponse {
                code: bmux_ipc::ErrorCode::InvalidRequest,
                message: "unsupported request in permissions service test bridge".to_string(),
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

    fn service_test_session_id() -> Uuid {
        Uuid::from_u128(0xaaaaaaaa_aaaa_aaaa_aaaa_aaaaaaaaaaaa)
    }

    fn service_test_context(
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
        capability: &str,
        kind: ServiceKind,
        data_dir: &PathBuf,
    ) -> NativeServiceContext {
        let host_services = vec![
            RegisteredService {
                capability: HostScope::new("bmux.sessions.read").expect("capability should parse"),
                kind: ServiceKind::Query,
                interface_id: "session-query/v1".to_string(),
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
            plugin_id: "bmux.permissions".to_string(),
            request: ServiceRequest {
                caller_plugin_id: "test.caller".to_string(),
                service: RegisteredService {
                    capability: HostScope::new(capability).expect("capability should parse"),
                    kind,
                    interface_id: interface_id.to_string(),
                    provider: ProviderId::Plugin("bmux.permissions".to_string()),
                },
                operation: operation.to_string(),
                payload,
            },
            required_capabilities: vec![
                "bmux.commands".to_string(),
                "bmux.sessions.read".to_string(),
                "bmux.clients.read".to_string(),
                "bmux.storage".to_string(),
            ],
            provided_capabilities: vec![
                "bmux.permissions.read".to_string(),
                "bmux.permissions.write".to_string(),
                "bmux.sessions.policy".to_string(),
            ],
            services: host_services,
            available_capabilities: vec![
                "bmux.sessions.read".to_string(),
                "bmux.clients.read".to_string(),
                "bmux.storage".to_string(),
            ],
            enabled_plugins: vec!["bmux.permissions".to_string()],
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
                data_dir: data_dir.to_string_lossy().into_owned(),
            },
            settings: std::collections::BTreeMap::new(),
            plugin_settings_map: std::collections::BTreeMap::new(),
            host_kernel_bridge: Some(HostKernelBridge::from_fn(service_test_kernel_bridge)),
        }
    }

    fn service_test_data_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bmux-permissions-service-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("test data dir should be creatable");
        dir
    }

    #[test]
    fn resolve_current_session_uses_selected_session_name() {
        let alpha_id = Uuid::new_v4();
        let beta_id = Uuid::new_v4();
        let host = MockHost::with_sessions_and_selected(
            vec![
                SessionSummary {
                    id: alpha_id,
                    name: Some("alpha".to_string()),
                    client_count: 1,
                },
                SessionSummary {
                    id: beta_id,
                    name: Some("beta".to_string()),
                    client_count: 1,
                },
            ],
            Some(beta_id),
        );
        let session = resolve_current_session(&host).expect("active session should resolve");
        assert_eq!(session, "beta");
    }

    #[test]
    fn resolve_current_session_falls_back_when_selected_session_missing() {
        let alpha_id = Uuid::new_v4();
        let host = MockHost::with_sessions_and_selected(
            vec![SessionSummary {
                id: alpha_id,
                name: Some("alpha".to_string()),
                client_count: 1,
            }],
            Some(Uuid::new_v4()),
        );
        let session =
            resolve_current_session(&host).expect("fallback to first listed session should work");
        assert_eq!(session, "alpha");
    }

    #[test]
    fn resolve_current_session_requires_available_sessions() {
        let host = MockHost::with_sessions(Vec::new());
        let error = resolve_current_session(&host)
            .expect_err("missing sessions should produce an actionable error");
        assert!(error.contains("no active session"));
    }

    #[test]
    fn observer_role_denies_mutation_and_admin() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        grant_entry(
            &host,
            GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "observer".to_string(),
            },
        )
        .expect("grant should succeed");

        let mutation = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "mutation".to_string(),
            },
        )
        .expect("policy evaluation should succeed");
        assert!(!mutation.allowed);

        let admin = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "admin".to_string(),
            },
        )
        .expect("policy evaluation should succeed");
        assert!(!admin.allowed);
    }

    #[test]
    fn writer_role_allows_mutation_but_denies_admin() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        grant_entry(
            &host,
            GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "writer".to_string(),
            },
        )
        .expect("grant should succeed");

        let mutation = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "mutation".to_string(),
            },
        )
        .expect("policy evaluation should succeed");
        assert!(mutation.allowed);

        let admin = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "admin".to_string(),
            },
        )
        .expect("policy evaluation should succeed");
        assert!(!admin.allowed);
    }

    #[test]
    fn owner_role_allows_admin() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        grant_entry(
            &host,
            GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "owner".to_string(),
            },
        )
        .expect("grant should succeed");

        let admin = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "admin".to_string(),
            },
        )
        .expect("policy evaluation should succeed");
        assert!(admin.allowed);
    }

    #[test]
    fn observer_role_denies_granular_mutation_action() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        grant_entry(
            &host,
            GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "observer".to_string(),
            },
        )
        .expect("grant should succeed");

        let decision = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "context.close".to_string(),
            },
        )
        .expect("policy evaluation should succeed");
        assert!(!decision.allowed);
    }

    #[test]
    fn writer_role_allows_granular_mutation_action() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        grant_entry(
            &host,
            GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "writer".to_string(),
            },
        )
        .expect("grant should succeed");

        let decision = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "context.select".to_string(),
            },
        )
        .expect("policy evaluation should succeed");
        assert!(decision.allowed);
    }

    #[test]
    fn policy_rejects_legacy_alias_action_names() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        grant_entry(
            &host,
            GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "writer".to_string(),
            },
        )
        .expect("grant should succeed");

        let decision = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "pane_split".to_string(),
            },
        )
        .expect("policy evaluation should succeed");

        assert!(!decision.allowed);
        assert!(
            decision
                .reason
                .is_some_and(|reason| reason.contains("invalid session policy action"))
        );
    }

    #[test]
    fn missing_entry_defaults_to_allow() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        let decision = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "mutation".to_string(),
            },
        )
        .expect("policy evaluation should succeed");
        assert!(decision.allowed);
    }

    #[test]
    fn revoke_removes_entry_from_policy_state() {
        let session_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let host = MockHost::with_session(session_id, "alpha");

        grant_entry(
            &host,
            GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "observer".to_string(),
            },
        )
        .expect("grant should succeed");
        revoke_entry(
            &host,
            RevokeRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
            },
        )
        .expect("revoke should succeed");

        let decision = evaluate_policy(
            &host,
            &SessionPolicyCheckRequest {
                session_id,
                client_id,
                principal_id: Uuid::new_v4(),
                action: "admin".to_string(),
            },
        )
        .expect("policy evaluation should succeed");
        assert!(decision.allowed);
    }

    #[test]
    fn invoke_service_grant_list_and_revoke_roundtrip() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let client_id = Uuid::new_v4().to_string();

        let grant_context = service_test_context(
            "permission-command/v1",
            "grant",
            encode_service_message(&GrantRequest {
                session: "alpha".to_string(),
                client: client_id.clone(),
                role: "observer".to_string(),
            })
            .expect("grant request should encode"),
            "bmux.permissions.write",
            ServiceKind::Command,
            &data_dir,
        );
        let grant = plugin.invoke_service(grant_context);
        assert!(
            grant.error.is_none(),
            "unexpected grant error: {:?}",
            grant.error
        );

        let list_context = service_test_context(
            "permission-query/v1",
            "list",
            encode_service_message(&ListPermissionsRequest {
                session: "alpha".to_string(),
            })
            .expect("list request should encode"),
            "bmux.permissions.read",
            ServiceKind::Query,
            &data_dir,
        );
        let listed = plugin.invoke_service(list_context);
        assert!(
            listed.error.is_none(),
            "unexpected list error: {:?}",
            listed.error
        );
        let listed_payload: ListPermissionsResponse =
            decode_service_message(&listed.payload).expect("list response should decode");
        assert_eq!(listed_payload.entries.len(), 1);
        assert_eq!(listed_payload.entries[0].client_id, client_id);
        assert_eq!(listed_payload.entries[0].role, "observer");

        let revoke_context = service_test_context(
            "permission-command/v1",
            "revoke",
            encode_service_message(&RevokeRequest {
                session: "alpha".to_string(),
                client: listed_payload.entries[0].client_id.clone(),
            })
            .expect("revoke request should encode"),
            "bmux.permissions.write",
            ServiceKind::Command,
            &data_dir,
        );
        let revoke = plugin.invoke_service(revoke_context);
        assert!(
            revoke.error.is_none(),
            "unexpected revoke error: {:?}",
            revoke.error
        );

        let relist_context = service_test_context(
            "permission-query/v1",
            "list",
            encode_service_message(&ListPermissionsRequest {
                session: "alpha".to_string(),
            })
            .expect("list request should encode"),
            "bmux.permissions.read",
            ServiceKind::Query,
            &data_dir,
        );
        let relisted = plugin.invoke_service(relist_context);
        assert!(
            relisted.error.is_none(),
            "unexpected relist error: {:?}",
            relisted.error
        );
        let relisted_payload: ListPermissionsResponse =
            decode_service_message(&relisted.payload).expect("relist response should decode");
        assert!(relisted_payload.entries.is_empty());
    }

    #[test]
    fn invoke_service_policy_check_denies_observer_mutation() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let client_id = Uuid::new_v4();

        let grant_context = service_test_context(
            "permission-command/v1",
            "grant",
            encode_service_message(&GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "observer".to_string(),
            })
            .expect("grant request should encode"),
            "bmux.permissions.write",
            ServiceKind::Command,
            &data_dir,
        );
        let grant = plugin.invoke_service(grant_context);
        assert!(
            grant.error.is_none(),
            "unexpected grant error: {:?}",
            grant.error
        );

        let policy_context = service_test_context(
            "session-policy-query/v1",
            "check",
            encode_service_message(&SessionPolicyCheckRequest {
                session_id: service_test_session_id(),
                client_id,
                principal_id: Uuid::new_v4(),
                action: "mutation".to_string(),
            })
            .expect("policy request should encode"),
            "bmux.sessions.policy",
            ServiceKind::Query,
            &data_dir,
        );
        let policy = plugin.invoke_service(policy_context);
        assert!(
            policy.error.is_none(),
            "unexpected policy error: {:?}",
            policy.error
        );
        let decision: SessionPolicyCheckResponse =
            decode_service_message(&policy.payload).expect("policy response should decode");
        assert!(!decision.allowed);
        assert!(decision.reason.is_some());
    }

    #[test]
    fn invoke_service_grant_rejects_invalid_role() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let context = service_test_context(
            "permission-command/v1",
            "grant",
            encode_service_message(&GrantRequest {
                session: "alpha".to_string(),
                client: Uuid::new_v4().to_string(),
                role: "invalid".to_string(),
            })
            .expect("grant request should encode"),
            "bmux.permissions.write",
            ServiceKind::Command,
            &data_dir,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected grant failure");
        assert_eq!(error.code, "grant_failed");
        assert!(error.message.contains("invalid role"));
    }

    #[test]
    fn invoke_service_rejects_invalid_grant_payload() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let context = service_test_context(
            "permission-command/v1",
            "grant",
            vec![1, 2, 3],
            "bmux.permissions.write",
            ServiceKind::Command,
            &data_dir,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected invalid request error");
        assert_eq!(error.code, "invalid_request");
    }

    #[test]
    fn invoke_service_list_reports_missing_session() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let context = service_test_context(
            "permission-query/v1",
            "list",
            encode_service_message(&ListPermissionsRequest {
                session: "missing-session".to_string(),
            })
            .expect("list request should encode"),
            "bmux.permissions.read",
            ServiceKind::Query,
            &data_dir,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected list failure");
        assert_eq!(error.code, "list_failed");
        assert!(error.message.contains("not found"));
    }

    #[test]
    fn invoke_service_policy_defaults_to_allow_without_entry() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let context = service_test_context(
            "session-policy-query/v1",
            "check",
            encode_service_message(&SessionPolicyCheckRequest {
                session_id: service_test_session_id(),
                client_id: Uuid::new_v4(),
                principal_id: Uuid::new_v4(),
                action: "mutation".to_string(),
            })
            .expect("policy request should encode"),
            "bmux.sessions.policy",
            ServiceKind::Query,
            &data_dir,
        );

        let response = plugin.invoke_service(context);
        assert!(
            response.error.is_none(),
            "unexpected policy error: {:?}",
            response.error
        );
        let decision: SessionPolicyCheckResponse =
            decode_service_message(&response.payload).expect("policy response should decode");
        assert!(decision.allowed);
        assert!(decision.reason.is_none());
    }

    #[test]
    fn invoke_service_rejects_invalid_policy_payload() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let context = service_test_context(
            "session-policy-query/v1",
            "check",
            vec![1, 2, 3],
            "bmux.sessions.policy",
            ServiceKind::Query,
            &data_dir,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected invalid request");
        assert_eq!(error.code, "invalid_request");
    }

    #[test]
    fn invoke_service_rejects_unsupported_operation() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let context = service_test_context(
            "permission-command/v1",
            "unknown",
            Vec::new(),
            "bmux.permissions.write",
            ServiceKind::Command,
            &data_dir,
        );

        let response = plugin.invoke_service(context);
        let error = response
            .error
            .expect("expected unsupported operation error");
        assert_eq!(error.code, "unsupported_service_operation");
    }

    #[test]
    fn invoke_service_policy_denies_unknown_action() {
        let mut plugin = PermissionsPlugin;
        let data_dir = service_test_data_dir();
        let client_id = Uuid::new_v4();

        let grant_context = service_test_context(
            "permission-command/v1",
            "grant",
            encode_service_message(&GrantRequest {
                session: "alpha".to_string(),
                client: client_id.to_string(),
                role: "observer".to_string(),
            })
            .expect("grant request should encode"),
            "bmux.permissions.write",
            ServiceKind::Command,
            &data_dir,
        );
        let grant = plugin.invoke_service(grant_context);
        assert!(
            grant.error.is_none(),
            "unexpected grant error: {:?}",
            grant.error
        );

        let context = service_test_context(
            "session-policy-query/v1",
            "check",
            encode_service_message(&SessionPolicyCheckRequest {
                session_id: service_test_session_id(),
                client_id,
                principal_id: Uuid::new_v4(),
                action: "unknown-action".to_string(),
            })
            .expect("policy request should encode"),
            "bmux.sessions.policy",
            ServiceKind::Query,
            &data_dir,
        );

        let response = plugin.invoke_service(context);
        assert!(
            response.error.is_none(),
            "unexpected policy error: {:?}",
            response.error
        );
        let decision: SessionPolicyCheckResponse =
            decode_service_message(&response.payload).expect("policy response should decode");
        assert!(!decision.allowed);
        assert!(
            decision
                .reason
                .is_some_and(|reason| reason.contains("invalid session policy action"))
        );
    }
}
