#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_client::BmuxClient;
use bmux_config::ConfigPaths;
use bmux_ipc::{SessionPermissionSummary, SessionRole, SessionSelector};
use bmux_plugin::{
    CommandExecutionKind, HostScope, NativeCommandContext, NativeDescriptor, NativeServiceContext,
    PluginCommand, PluginCommandArgument, PluginCommandArgumentKind, PluginFeature, PluginService,
    RustPlugin, ServiceKind, ServiceResponse, decode_service_message, encode_service_message,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Default)]
struct PermissionsPlugin;

impl RustPlugin for PermissionsPlugin {
    fn descriptor(&self) -> NativeDescriptor {
        NativeDescriptor {
            id: "bmux.permissions".to_string(),
            display_name: "bmux Permissions".to_string(),
            plugin_version: env!("CARGO_PKG_VERSION").to_string(),
            plugin_api: bmux_plugin::PluginManifestCompatibility {
                minimum: "1.0".to_string(),
                maximum: None,
            },
            native_abi: bmux_plugin::PluginManifestCompatibility {
                minimum: "1.0".to_string(),
                maximum: None,
            },
            description: Some("Shipped bmux permissions command plugin".to_string()),
            homepage: None,
            required_capabilities: BTreeSet::from([
                HostScope::new("bmux.commands").expect("host scope should parse")
            ]),
            provided_capabilities: BTreeSet::from([
                HostScope::new("bmux.permissions.read").expect("host scope should parse"),
                HostScope::new("bmux.permissions.write").expect("host scope should parse"),
            ]),
            provided_features: BTreeSet::from([
                PluginFeature::new("bmux.permissions").expect("plugin feature should parse")
            ]),
            services: vec![
                PluginService {
                    capability: HostScope::new("bmux.permissions.read")
                        .expect("host scope should parse"),
                    kind: ServiceKind::Query,
                    interface_id: "permission-query/v1".to_string(),
                },
                PluginService {
                    capability: HostScope::new("bmux.permissions.write")
                        .expect("host scope should parse"),
                    kind: ServiceKind::Command,
                    interface_id: "permission-command/v1".to_string(),
                },
            ],
            commands: vec![
                PluginCommand::new(
                    "permissions",
                    "List explicit role assignments for a session",
                )
                .path(["permissions"])
                .alias(["session", "permissions"])
                .argument(
                    PluginCommandArgument::option("session", PluginCommandArgumentKind::String)
                        .required(true)
                        .short('s'),
                )
                .argument(PluginCommandArgument::flag("json").short('j'))
                .argument(PluginCommandArgument::flag("watch").short('w'))
                .execution(CommandExecutionKind::ProviderExec)
                .expose_in_cli(true),
                PluginCommand::new("grant", "Grant a role to a client in a session")
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
                PluginCommand::new(
                    "revoke",
                    "Revoke an explicit role from a client in a session",
                )
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
            ],
            event_subscriptions: Vec::new(),
            dependencies: Vec::new(),
            lifecycle: bmux_plugin::PluginLifecycle {
                activate_on_startup: false,
                receive_events: false,
                allow_hot_reload: true,
            },
        }
    }

    fn run_command(&mut self, context: NativeCommandContext) -> i32 {
        match context.command.as_str() {
            "permissions" => run_permissions_command(&context),
            "grant" => run_grant_command(&context),
            "revoke" => run_revoke_command(&context),
            _ => 64,
        }
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match (
            context.request.service.interface_id.as_str(),
            context.request.operation.as_str(),
        ) {
            ("permission-query/v1", "list") => run_permission_query_service(&context),
            ("permission-command/v1", "grant") => run_permission_command_service(&context),
            ("permission-command/v1", "revoke") => run_permission_command_service(&context),
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

bmux_plugin::export_plugin!(PermissionsPlugin);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListPermissionsRequest {
    session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListPermissionsResponse {
    permissions: Vec<SessionPermissionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GrantPermissionRequest {
    session: String,
    client_id: Uuid,
    role: SessionRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GrantPermissionResponse {
    client_id: Uuid,
    role: SessionRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RevokePermissionRequest {
    session: String,
    client_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RevokePermissionResponse {
    client_id: Uuid,
}

fn run_permission_query_service(context: &NativeServiceContext) -> ServiceResponse {
    let request = match decode_service_message::<ListPermissionsRequest>(&context.request.payload) {
        Ok(request) => request,
        Err(error) => {
            return ServiceResponse::error("invalid_request", error.to_string());
        }
    };

    let paths = ConfigPaths::new(
        context.connection.config_dir.clone().into(),
        context.connection.runtime_dir.clone().into(),
        context.connection.data_dir.clone().into(),
    );

    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| {
            handle.block_on(async_list_permissions_service(&paths, &request.session))
        }),
        Err(_) => match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => {
                runtime.block_on(async_list_permissions_service(&paths, &request.session))
            }
            Err(error) => ServiceResponse::error("runtime_error", error.to_string()),
        },
    }
}

async fn async_list_permissions_service(paths: &ConfigPaths, session: &str) -> ServiceResponse {
    let selector = parse_session_selector(session);
    match BmuxClient::connect_with_paths(paths, "bmux-permissions-service").await {
        Ok(mut client) => match client.list_permissions(selector).await {
            Ok(permissions) => {
                match encode_service_message(&ListPermissionsResponse { permissions }) {
                    Ok(payload) => ServiceResponse::ok(payload),
                    Err(error) => ServiceResponse::error("encode_error", error.to_string()),
                }
            }
            Err(error) => ServiceResponse::error("list_failed", error.to_string()),
        },
        Err(error) => ServiceResponse::error("connect_failed", error.to_string()),
    }
}

fn run_permission_command_service(context: &NativeServiceContext) -> ServiceResponse {
    let paths = ConfigPaths::new(
        context.connection.config_dir.clone().into(),
        context.connection.runtime_dir.clone().into(),
        context.connection.data_dir.clone().into(),
    );

    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| {
            handle.block_on(async_permission_command_service(
                &paths,
                context.request.operation.as_str(),
                &context.request.payload,
            ))
        }),
        Err(_) => match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(async_permission_command_service(
                &paths,
                context.request.operation.as_str(),
                &context.request.payload,
            )),
            Err(error) => ServiceResponse::error("runtime_error", error.to_string()),
        },
    }
}

async fn async_permission_command_service(
    paths: &ConfigPaths,
    operation: &str,
    payload: &[u8],
) -> ServiceResponse {
    match operation {
        "grant" => {
            let request = match decode_service_message::<GrantPermissionRequest>(payload) {
                Ok(request) => request,
                Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
            };
            let selector = parse_session_selector(&request.session);
            let mut client = match connect_client(paths).await {
                Ok(client) => client,
                Err(code) => {
                    return ServiceResponse::error(
                        "connect_failed",
                        format!("client error code {code}"),
                    );
                }
            };
            match client
                .grant_role(selector, request.client_id, request.role)
                .await
            {
                Ok(()) => match encode_service_message(&GrantPermissionResponse {
                    client_id: request.client_id,
                    role: request.role,
                }) {
                    Ok(payload) => ServiceResponse::ok(payload),
                    Err(error) => ServiceResponse::error("encode_error", error.to_string()),
                },
                Err(error) => ServiceResponse::error("grant_failed", error.to_string()),
            }
        }
        "revoke" => {
            let request = match decode_service_message::<RevokePermissionRequest>(payload) {
                Ok(request) => request,
                Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
            };
            let selector = parse_session_selector(&request.session);
            let mut client = match connect_client(paths).await {
                Ok(client) => client,
                Err(code) => {
                    return ServiceResponse::error(
                        "connect_failed",
                        format!("client error code {code}"),
                    );
                }
            };
            match client.revoke_role(selector, request.client_id).await {
                Ok(()) => match encode_service_message(&RevokePermissionResponse {
                    client_id: request.client_id,
                }) {
                    Ok(payload) => ServiceResponse::ok(payload),
                    Err(error) => ServiceResponse::error("encode_error", error.to_string()),
                },
                Err(error) => ServiceResponse::error("revoke_failed", error.to_string()),
            }
        }
        _ => ServiceResponse::error(
            "unsupported_service_operation",
            format!("unsupported permissions command operation '{operation}'"),
        ),
    }
}

fn run_permissions_command(context: &NativeCommandContext) -> i32 {
    let mut session = None;
    let mut as_json = false;
    let mut watch = false;
    let mut iter = context.arguments.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--session" => session = iter.next().cloned(),
            "--json" => as_json = true,
            "--watch" => watch = true,
            _ => return 64,
        }
    }
    let Some(session) = session else {
        eprintln!("permissions requires --session <name-or-uuid>");
        return 64;
    };

    let paths = ConfigPaths::new(
        context.connection.config_dir.clone().into(),
        context.connection.runtime_dir.clone().into(),
        context.connection.data_dir.clone().into(),
    );

    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| {
            handle.block_on(async_permissions_command(&paths, &session, as_json, watch))
        }),
        Err(_) => match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => {
                runtime.block_on(async_permissions_command(&paths, &session, as_json, watch))
            }
            Err(_) => 70,
        },
    }
}

async fn async_permissions_command(
    paths: &ConfigPaths,
    session: &str,
    as_json: bool,
    watch: bool,
) -> i32 {
    let selector = parse_session_selector(session);
    let mut client = match BmuxClient::connect_with_paths(paths, "bmux-permissions-plugin").await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("failed connecting to bmux host: {error}");
            return 1;
        }
    };

    if watch {
        println!("watching permissions for session '{session}' (Ctrl-C to stop)");
        let mut last_permissions: Option<Vec<SessionPermissionSummary>> = None;
        loop {
            match client.list_permissions(selector.clone()).await {
                Ok(permissions) => {
                    if last_permissions.as_ref() != Some(&permissions) {
                        render_permissions(&permissions, false);
                        last_permissions = Some(permissions);
                    }
                }
                Err(error) => {
                    eprintln!("failed listing permissions: {error}");
                    return 1;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
    }

    match client.list_permissions(selector).await {
        Ok(permissions) => {
            render_permissions(&permissions, as_json);
            0
        }
        Err(error) => {
            eprintln!("failed listing permissions: {error}");
            1
        }
    }
}

fn run_grant_command(context: &NativeCommandContext) -> i32 {
    let mut session = None;
    let mut client = None;
    let mut role = None;
    let mut iter = context.arguments.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--session" => session = iter.next().cloned(),
            "--client" => client = iter.next().cloned(),
            "--role" => role = iter.next().cloned(),
            _ => return 64,
        }
    }

    let (Some(session), Some(client), Some(role)) = (session, client, role) else {
        eprintln!("grant requires --session <name-or-uuid> --client <uuid> --role <role>");
        return 64;
    };
    let client_id = match uuid::Uuid::parse_str(&client) {
        Ok(client_id) => client_id,
        Err(_) => {
            eprintln!("invalid client id: {client}");
            return 64;
        }
    };
    let role = match parse_role(&role) {
        Some(role) => role,
        None => {
            eprintln!("invalid role: {role}");
            return 64;
        }
    };

    let paths = command_paths(context);
    match block_on_plugin_future(async move {
        async_grant_command(&paths, &session, client_id, role).await
    }) {
        Ok(code) => code,
        Err(_) => 70,
    }
}

async fn async_grant_command(
    paths: &ConfigPaths,
    session: &str,
    client_id: uuid::Uuid,
    role: SessionRole,
) -> i32 {
    let selector = parse_session_selector(session);
    let mut client = match connect_client(paths).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.grant_role(selector, client_id, role).await {
        Ok(()) => {
            println!("granted role {} to client {}", role_label(role), client_id);
            0
        }
        Err(error) => {
            eprintln!("failed granting role: {error}");
            1
        }
    }
}

fn run_revoke_command(context: &NativeCommandContext) -> i32 {
    let mut session = None;
    let mut client = None;
    let mut iter = context.arguments.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--session" => session = iter.next().cloned(),
            "--client" => client = iter.next().cloned(),
            _ => return 64,
        }
    }

    let (Some(session), Some(client)) = (session, client) else {
        eprintln!("revoke requires --session <name-or-uuid> --client <uuid>");
        return 64;
    };
    let client_id = match uuid::Uuid::parse_str(&client) {
        Ok(client_id) => client_id,
        Err(_) => {
            eprintln!("invalid client id: {client}");
            return 64;
        }
    };

    let paths = command_paths(context);
    match block_on_plugin_future(
        async move { async_revoke_command(&paths, &session, client_id).await },
    ) {
        Ok(code) => code,
        Err(_) => 70,
    }
}

async fn async_revoke_command(paths: &ConfigPaths, session: &str, client_id: uuid::Uuid) -> i32 {
    let selector = parse_session_selector(session);
    let mut client = match connect_client(paths).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.revoke_role(selector, client_id).await {
        Ok(()) => {
            println!("revoked explicit role for client {client_id}");
            0
        }
        Err(error) => {
            eprintln!("failed revoking role: {error}");
            1
        }
    }
}

fn command_paths(context: &NativeCommandContext) -> ConfigPaths {
    ConfigPaths::new(
        context.connection.config_dir.clone().into(),
        context.connection.runtime_dir.clone().into(),
        context.connection.data_dir.clone().into(),
    )
}

async fn connect_client(paths: &ConfigPaths) -> Result<BmuxClient, i32> {
    BmuxClient::connect_with_paths(paths, "bmux-permissions-plugin")
        .await
        .map_err(|error| {
            eprintln!("failed connecting to bmux host: {error}");
            1
        })
}

fn block_on_plugin_future<F>(future: F) -> Result<i32, ()>
where
    F: std::future::Future<Output = i32>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => Ok(tokio::task::block_in_place(|| handle.block_on(future))),
        Err(_) => match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => Ok(runtime.block_on(future)),
            Err(_) => Err(()),
        },
    }
}

fn parse_role(value: &str) -> Option<SessionRole> {
    match value {
        "owner" => Some(SessionRole::Owner),
        "writer" => Some(SessionRole::Writer),
        "observer" => Some(SessionRole::Observer),
        _ => None,
    }
}

fn render_permissions(permissions: &[SessionPermissionSummary], as_json: bool) {
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(permissions).expect("permissions json should encode")
        );
        return;
    }

    if permissions.is_empty() {
        println!("no explicit role assignments");
        return;
    }

    println!("CLIENT_ID                            ROLE");
    for permission in permissions {
        println!(
            "{:<36} {}",
            permission.client_id,
            role_label(permission.role)
        );
    }
}

fn role_label(role: SessionRole) -> &'static str {
    match role {
        SessionRole::Owner => "owner",
        SessionRole::Writer => "writer",
        SessionRole::Observer => "observer",
    }
}

fn parse_session_selector(value: &str) -> SessionSelector {
    match uuid::Uuid::parse_str(value) {
        Ok(id) => SessionSelector::ById(id),
        Err(_) => SessionSelector::ByName(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::PermissionsPlugin;
    use bmux_plugin::RustPlugin;

    #[test]
    fn descriptor_parses() {
        let descriptor = PermissionsPlugin.descriptor();
        let serialized = descriptor
            .to_toml_string()
            .expect("descriptor should serialize");
        let reparsed = bmux_plugin::NativeDescriptor::from_toml_str(&serialized)
            .expect("descriptor should parse");
        assert_eq!(reparsed.id, "bmux.permissions");
        assert_eq!(reparsed.commands.len(), 3);
        assert!(
            reparsed
                .commands
                .iter()
                .all(|command| command.expose_in_cli)
        );
        assert!(reparsed.commands.iter().any(|command| {
            command.name == "permissions"
                && command.aliases == vec![vec!["session".to_string(), "permissions".to_string()]]
        }));
        assert!(reparsed.commands.iter().any(|command| {
            command.name == "grant"
                && command.aliases == vec![vec!["session".to_string(), "grant".to_string()]]
        }));
        assert!(reparsed.commands.iter().any(|command| {
            command.name == "revoke"
                && command.aliases == vec![vec!["session".to_string(), "revoke".to_string()]]
        }));
    }

    #[test]
    fn exported_entrypoint_returns_pointer() {
        assert!(!crate::bmux_plugin_entry_v1().is_null());
    }
}
