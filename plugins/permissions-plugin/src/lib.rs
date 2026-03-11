#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_cli_output::{Table, TableColumn, write_table};
use bmux_client::BmuxClient;
use bmux_config::ConfigPaths;
use bmux_ipc::{SessionPermissionSummary, SessionRole, SessionSelector};
use bmux_plugin::{
    CommandExecutionKind, HostScope, NativeCommandContext, NativeDescriptor, NativeServiceContext,
    PluginCommand, PluginCommandArgument, PluginCommandArgumentKind, PluginService, RustPlugin,
    ServiceKind, ServiceResponse, decode_service_message, encode_service_message,
};
use serde::{Deserialize, Serialize};
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
            )
            .command(
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
            )
            .command(
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
            )
            .lifecycle(bmux_plugin::PluginLifecycle {
                activate_on_startup: false,
                receive_events: false,
                allow_hot_reload: true,
            })
            .build()
            .expect("descriptor should validate")
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
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let selector = parse_session_selector(&request.session);
    with_client(
        context,
        "bmux-permissions-plugin-service",
        move |mut client| async move {
            let permissions = client
                .list_permissions(selector)
                .await
                .map_err(client_error)?;
            Ok(encode_service_message(&ListPermissionsResponse {
                permissions,
            })?)
        },
    )
}

fn run_permission_command_service(context: &NativeServiceContext) -> ServiceResponse {
    match context.request.operation.as_str() {
        "grant" => {
            let request =
                match decode_service_message::<GrantPermissionRequest>(&context.request.payload) {
                    Ok(request) => request,
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error.to_string());
                    }
                };
            let selector = parse_session_selector(&request.session);
            with_client(
                context,
                "bmux-permissions-plugin-service",
                move |mut client| async move {
                    client
                        .grant_role(selector, request.client_id, request.role)
                        .await
                        .map_err(client_error)?;
                    Ok(encode_service_message(&GrantPermissionResponse {
                        client_id: request.client_id,
                        role: request.role,
                    })?)
                },
            )
        }
        "revoke" => {
            let request =
                match decode_service_message::<RevokePermissionRequest>(&context.request.payload) {
                    Ok(request) => request,
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error.to_string());
                    }
                };
            let selector = parse_session_selector(&request.session);
            with_client(
                context,
                "bmux-permissions-plugin-service",
                move |mut client| async move {
                    client
                        .revoke_role(selector, request.client_id)
                        .await
                        .map_err(client_error)?;
                    Ok(encode_service_message(&RevokePermissionResponse {
                        client_id: request.client_id,
                    })?)
                },
            )
        }
        _ => ServiceResponse::error(
            "unsupported_service_operation",
            format!(
                "unsupported permissions command operation '{}'",
                context.request.operation
            ),
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

    if watch {
        return watch_permissions(context, &session);
    }

    let selector = parse_session_selector(&session);
    let permissions = match with_command_client(
        context,
        "bmux-permissions-plugin-command",
        |mut client| async move {
            client
                .list_permissions(selector)
                .await
                .map_err(|error| error.to_string())
        },
    ) {
        Ok(permissions) => permissions,
        Err(error) => {
            eprintln!("failed listing permissions: {error}");
            return 1;
        }
    };
    render_permissions(&permissions, as_json);
    0
}

fn watch_permissions(context: &NativeCommandContext, session: &str) -> i32 {
    println!("watching permissions for session '{session}' (Ctrl-C to stop)");
    let mut last_permissions: Option<Vec<SessionPermissionSummary>> = None;
    loop {
        let selector = parse_session_selector(session);
        match with_command_client(
            context,
            "bmux-permissions-plugin-command",
            |mut client| async move {
                client
                    .list_permissions(selector)
                    .await
                    .map_err(|error| error.to_string())
            },
        ) {
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
        std::thread::sleep(std::time::Duration::from_millis(500));
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
    let client_id = match Uuid::parse_str(&client) {
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

    let selector = parse_session_selector(&session);
    match with_command_client(
        context,
        "bmux-permissions-plugin-command",
        |mut client| async move {
            client
                .grant_role(selector, client_id, role)
                .await
                .map_err(|error| error.to_string())?;
            Ok(GrantPermissionResponse { client_id, role })
        },
    ) {
        Ok(response) => {
            println!(
                "granted role {} to client {}",
                role_label(response.role),
                response.client_id
            );
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
    let client_id = match Uuid::parse_str(&client) {
        Ok(client_id) => client_id,
        Err(_) => {
            eprintln!("invalid client id: {client}");
            return 64;
        }
    };

    let selector = parse_session_selector(&session);
    match with_command_client(
        context,
        "bmux-permissions-plugin-command",
        |mut client| async move {
            client
                .revoke_role(selector, client_id)
                .await
                .map_err(|error| error.to_string())?;
            Ok(RevokePermissionResponse { client_id })
        },
    ) {
        Ok(response) => {
            println!("revoked explicit role for client {}", response.client_id);
            0
        }
        Err(error) => {
            eprintln!("failed revoking role: {error}");
            1
        }
    }
}

fn with_client<F, Fut>(
    context: &NativeServiceContext,
    principal: &str,
    operation: F,
) -> ServiceResponse
where
    F: FnOnce(BmuxClient) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = bmux_plugin::Result<Vec<u8>>> + Send + 'static,
{
    let paths = ConfigPaths::new(
        context.connection.config_dir.clone().into(),
        context.connection.runtime_dir.clone().into(),
        context.connection.data_dir.clone().into(),
    );
    let principal = principal.to_string();

    match std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("runtime_error: {error}"))?;
        runtime.block_on(async move {
            let client = BmuxClient::connect_with_paths(&paths, &principal)
                .await
                .map_err(|error| format!("connect_failed: {error}"))?;
            operation(client)
                .await
                .map_err(|error| format!("service_failed: {error}"))
        })
    })
    .join()
    {
        Ok(Ok(payload)) => ServiceResponse::ok(payload),
        Ok(Err(error)) => {
            let mut split = error.splitn(2, ':');
            let code = split.next().unwrap_or("service_failed").trim();
            let message = split.next().unwrap_or("service invocation failed").trim();
            ServiceResponse::error(code, message)
        }
        Err(_) => ServiceResponse::error("runtime_error", "service worker thread panicked"),
    }
}

fn with_command_client<T, Fut>(
    context: &NativeCommandContext,
    principal: &str,
    operation: impl FnOnce(BmuxClient) -> Fut,
) -> Result<T, String>
where
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let paths = ConfigPaths::new(
        context.connection.config_dir.clone().into(),
        context.connection.runtime_dir.clone().into(),
        context.connection.data_dir.clone().into(),
    );
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| {
            handle.block_on(async {
                let client = BmuxClient::connect_with_paths(&paths, principal)
                    .await
                    .map_err(|error| error.to_string())?;
                operation(client).await
            })
        }),
        Err(_) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime.block_on(async {
                let client = BmuxClient::connect_with_paths(&paths, principal)
                    .await
                    .map_err(|error| error.to_string())?;
                operation(client).await
            })
        }
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

    let mut table = Table::new(vec![
        TableColumn::new("CLIENT_ID").min_width(36),
        TableColumn::new("ROLE"),
    ]);
    for permission in permissions {
        table.push_row(vec![
            permission.client_id.to_string(),
            role_label(permission.role).to_string(),
        ]);
    }

    let mut stdout = std::io::stdout().lock();
    if let Err(error) = write_table(&mut stdout, &table) {
        eprintln!("failed rendering permissions table: {error}");
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
    match Uuid::parse_str(value) {
        Ok(id) => SessionSelector::ById(id),
        Err(_) => SessionSelector::ByName(value.to_string()),
    }
}

fn client_error(error: bmux_client::ClientError) -> bmux_plugin::PluginError {
    bmux_plugin::PluginError::ServiceProtocol {
        details: error.to_string(),
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
    }

    #[test]
    fn exported_entrypoint_returns_pointer() {
        assert!(!crate::bmux_plugin_entry_v1().is_null());
    }
}
