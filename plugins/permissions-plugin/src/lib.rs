#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin::{
    CommandExecutionKind, HostRuntimeApi, HostScope, NativeCommandContext, NativeDescriptor,
    NativeServiceContext, PluginCommand, PluginCommandArgument, PluginCommandArgumentKind,
    PluginService, RustPlugin, ServiceKind, ServiceResponse, SessionSelector,
    decode_service_message, encode_service_message,
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
            .provide_capability("bmux.permissions.read")
            .expect("capability should parse")
            .provide_capability("bmux.permissions.write")
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

fn load_state(caller: &impl HostRuntimeApi) -> Result<StoredPermissions, String> {
    let payload = caller
        .call_service_raw(
            "bmux.storage",
            ServiceKind::Query,
            "storage-query/v1",
            "get",
            encode_service_message(&StorageGetRequest {
                key: PERMISSIONS_STORAGE_KEY.to_string(),
            })
            .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
    let response: StorageGetResponse =
        decode_service_message(&payload).map_err(|error| error.to_string())?;
    match response.value {
        Some(value) => decode_service_message(&value).map_err(|error| error.to_string()),
        None => Ok(StoredPermissions::with_default()),
    }
}

fn save_state(caller: &impl HostRuntimeApi, state: &StoredPermissions) -> Result<(), String> {
    let value = encode_service_message(state).map_err(|error| error.to_string())?;
    caller
        .call_service_raw(
            "bmux.storage",
            ServiceKind::Command,
            "storage-command/v1",
            "set",
            encode_service_message(&StorageSetRequest {
                key: PERMISSIONS_STORAGE_KEY.to_string(),
                value,
            })
            .map_err(|error| error.to_string())?,
        )
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
struct StorageGetRequest {
    key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StorageGetResponse {
    value: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StorageSetRequest {
    key: String,
    value: Vec<u8>,
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
