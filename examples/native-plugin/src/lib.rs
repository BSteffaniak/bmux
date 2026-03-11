#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_cli_output::{Table, TableColumn, write_table};
use bmux_plugin::{
    CommandExecutionKind, HostScope, NativeCommandContext, NativeDescriptor, PluginCommand,
    PluginCommandArgument, PluginCommandArgumentKind, PluginEvent, PluginEventKind,
    PluginEventSubscription, PluginFeature, RustPlugin, ServiceKind,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Default)]
struct ExamplePlugin;

impl RustPlugin for ExamplePlugin {
    fn descriptor(&self) -> NativeDescriptor {
        NativeDescriptor {
            id: "example.native".to_string(),
            display_name: "Example Native Plugin".to_string(),
            plugin_version: env!("CARGO_PKG_VERSION").to_string(),
            plugin_api: bmux_plugin::PluginManifestCompatibility {
                minimum: "1.0".to_string(),
                maximum: None,
            },
            native_abi: bmux_plugin::PluginManifestCompatibility {
                minimum: "1.0".to_string(),
                maximum: None,
            },
            description: Some("Example in-repo native plugin for bmux".to_string()),
            homepage: None,
            provider_priority: 0,
            required_capabilities: BTreeSet::from([
                HostScope::new("bmux.commands").expect("host scope should parse"),
                HostScope::new("bmux.events.subscribe").expect("host scope should parse"),
                HostScope::new("bmux.config.read").expect("host scope should parse"),
                HostScope::new("bmux.storage").expect("host scope should parse"),
                HostScope::new("bmux.permissions.read").expect("host scope should parse"),
                HostScope::new("bmux.permissions.write").expect("host scope should parse"),
                HostScope::new("bmux.windows.read").expect("host scope should parse"),
                HostScope::new("bmux.windows.write").expect("host scope should parse"),
            ]),
            provided_capabilities: BTreeSet::new(),
            provided_features: BTreeSet::from([
                PluginFeature::new("example.native").expect("plugin feature should parse")
            ]),
            services: Vec::new(),
            commands: vec![
                PluginCommand::new("hello", "Print a hello message")
                    .argument(
                        PluginCommandArgument::positional(
                            "message",
                            PluginCommandArgumentKind::String,
                        )
                        .multiple(true)
                        .trailing_var_arg(true)
                        .allow_hyphen_values(true)
                        .summary("Optional greeting target"),
                    )
                    .execution(CommandExecutionKind::ProviderExec)
                    .expose_in_cli(true),
                PluginCommand::new(
                    "permissions-list",
                    "List session permissions through bmux provider service",
                )
                .argument(
                    PluginCommandArgument::positional("session", PluginCommandArgumentKind::String)
                        .required(true)
                        .summary("Session name or UUID"),
                )
                .execution(CommandExecutionKind::ProviderExec)
                .expose_in_cli(true),
                PluginCommand::new(
                    "permissions-grant",
                    "Grant a role through bmux provider service",
                )
                .argument(
                    PluginCommandArgument::positional("session", PluginCommandArgumentKind::String)
                        .required(true)
                        .summary("Session name or UUID"),
                )
                .argument(
                    PluginCommandArgument::option("client", PluginCommandArgumentKind::String)
                        .required(true)
                        .short('c')
                        .summary("Client UUID"),
                )
                .argument(
                    PluginCommandArgument::option("role", PluginCommandArgumentKind::Choice)
                        .required(true)
                        .short('r')
                        .choice_values(["owner", "writer", "observer"])
                        .summary("Role to grant"),
                )
                .execution(CommandExecutionKind::ProviderExec)
                .expose_in_cli(true),
                PluginCommand::new(
                    "permissions-revoke",
                    "Revoke a role through bmux provider service",
                )
                .argument(
                    PluginCommandArgument::positional("session", PluginCommandArgumentKind::String)
                        .required(true)
                        .summary("Session name or UUID"),
                )
                .argument(
                    PluginCommandArgument::option("client", PluginCommandArgumentKind::String)
                        .required(true)
                        .short('c')
                        .summary("Client UUID"),
                )
                .execution(CommandExecutionKind::ProviderExec)
                .expose_in_cli(true),
                PluginCommand::new(
                    "windows-list",
                    "List session windows through bmux provider service",
                )
                .argument(
                    PluginCommandArgument::positional("session", PluginCommandArgumentKind::String)
                        .required(true)
                        .summary("Session name or UUID"),
                )
                .execution(CommandExecutionKind::ProviderExec)
                .expose_in_cli(true),
                PluginCommand::new(
                    "windows-new",
                    "Create a session window through bmux provider service",
                )
                .argument(
                    PluginCommandArgument::positional("session", PluginCommandArgumentKind::String)
                        .required(true)
                        .summary("Session name or UUID"),
                )
                .argument(
                    PluginCommandArgument::option("name", PluginCommandArgumentKind::String)
                        .short('n')
                        .summary("Optional window name"),
                )
                .execution(CommandExecutionKind::ProviderExec)
                .expose_in_cli(true),
                PluginCommand::new(
                    "settings-show",
                    "Show plugin settings through bmux config service",
                )
                .execution(CommandExecutionKind::ProviderExec)
                .expose_in_cli(true),
                PluginCommand::new(
                    "storage-put",
                    "Store a key/value through bmux storage service",
                )
                .argument(
                    PluginCommandArgument::positional("key", PluginCommandArgumentKind::String)
                        .required(true)
                        .summary("Storage key"),
                )
                .argument(
                    PluginCommandArgument::positional("value", PluginCommandArgumentKind::String)
                        .position(1)
                        .required(true)
                        .multiple(true)
                        .trailing_var_arg(true)
                        .allow_hyphen_values(true)
                        .summary("Storage value"),
                )
                .execution(CommandExecutionKind::ProviderExec)
                .expose_in_cli(true),
                PluginCommand::new("storage-get", "Read a key through bmux storage service")
                    .argument(
                        PluginCommandArgument::positional("key", PluginCommandArgumentKind::String)
                            .required(true)
                            .summary("Storage key"),
                    )
                    .execution(CommandExecutionKind::ProviderExec)
                    .expose_in_cli(true),
            ],
            event_subscriptions: vec![PluginEventSubscription {
                kinds: BTreeSet::from([PluginEventKind::System, PluginEventKind::Window]),
                names: BTreeSet::from(["server_started".to_string(), "window_created".to_string()]),
            }],
            dependencies: vec![
                bmux_plugin::PluginDependency {
                    plugin_id: bmux_plugin::PluginId::new("bmux.permissions")
                        .expect("plugin id should parse"),
                    version_req: format!("={}", env!("CARGO_PKG_VERSION")),
                    required: true,
                },
                bmux_plugin::PluginDependency {
                    plugin_id: bmux_plugin::PluginId::new("bmux.windows")
                        .expect("plugin id should parse"),
                    version_req: format!("={}", env!("CARGO_PKG_VERSION")),
                    required: true,
                },
            ],
            lifecycle: bmux_plugin::PluginLifecycle {
                activate_on_startup: true,
                receive_events: true,
                allow_hot_reload: true,
            },
        }
    }

    fn run_command(&mut self, context: NativeCommandContext) -> i32 {
        match context.command.as_str() {
            "permissions-list" => run_permissions_list(&context),
            "permissions-grant" => run_permissions_grant(&context),
            "permissions-revoke" => run_permissions_revoke(&context),
            "windows-list" => run_windows_list(&context),
            "windows-new" => run_windows_new(&context),
            "settings-show" => run_settings_show(&context),
            "storage-put" => run_storage_put(&context),
            "storage-get" => run_storage_get(&context),
            "hello" => {
                if context.arguments.is_empty() {
                    println!("example.native: hello from bmux plugin");
                } else {
                    println!("example.native: hello {}", context.arguments.join(" "));
                }
                0
            }
            _ => 64,
        }
    }

    fn activate(&mut self, context: bmux_plugin::NativeLifecycleContext) -> i32 {
        println!("example.native: activated {}", context.plugin_id);
        0
    }

    fn deactivate(&mut self, context: bmux_plugin::NativeLifecycleContext) -> i32 {
        println!("example.native: deactivated {}", context.plugin_id);
        0
    }

    fn handle_event(&mut self, event: PluginEvent) -> i32 {
        println!("example.native: observed event {}", event.name);
        0
    }
}

bmux_plugin::export_plugin!(ExamplePlugin);

fn run_permissions_list(context: &NativeCommandContext) -> i32 {
    let Some(session) = context.arguments.first() else {
        eprintln!("example.native permissions-list requires a session name or UUID");
        return 64;
    };

    let response = match context.call_service::<ListPermissionsRequest, ListPermissionsResponse>(
        "bmux.permissions.read",
        ServiceKind::Query,
        "permission-query/v1",
        "list",
        &ListPermissionsRequest {
            session: session.to_string(),
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("example.native: failed listing permissions through service: {error}");
            return 1;
        }
    };

    if response.permissions.is_empty() {
        println!("example.native: no explicit role assignments");
    } else {
        println!("example.native permissions:");
        let mut table = Table::new(vec![
            TableColumn::new("CLIENT_ID").min_width(36),
            TableColumn::new("ROLE"),
        ]);
        for permission in response.permissions {
            table.push_row(vec![
                permission.client_id.to_string(),
                session_role_name(permission.role).to_string(),
            ]);
        }
        if let Err(error) = write_stdout_table(&table) {
            eprintln!("example.native: failed rendering permissions table: {error}");
            return 1;
        }
    }

    0
}

fn run_permissions_grant(context: &NativeCommandContext) -> i32 {
    let Some(session) = context.arguments.first() else {
        eprintln!("example.native permissions-grant requires a session name or UUID");
        return 64;
    };

    let mut client_id = None;
    let mut role = None;
    let mut args = context.arguments.iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--client" | "-c" => client_id = args.next().cloned(),
            "--role" | "-r" => role = args.next().cloned(),
            other => {
                eprintln!("example.native permissions-grant does not accept argument '{other}'");
                return 64;
            }
        }
    }

    let Some(client_id) = client_id else {
        eprintln!("example.native permissions-grant requires --client <uuid>");
        return 64;
    };
    let client_id = match uuid::Uuid::parse_str(&client_id) {
        Ok(value) => value,
        Err(_) => {
            eprintln!("example.native permissions-grant received invalid client id");
            return 64;
        }
    };
    let Some(role) = role else {
        eprintln!("example.native permissions-grant requires --role <role>");
        return 64;
    };
    let Some(role) = parse_role(&role) else {
        eprintln!("example.native permissions-grant received invalid role '{role}'");
        return 64;
    };

    let response = match context.call_service::<GrantPermissionRequest, GrantPermissionResponse>(
        "bmux.permissions.write",
        ServiceKind::Command,
        "permission-command/v1",
        "grant",
        &GrantPermissionRequest {
            session: session.to_string(),
            client_id,
            role,
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("example.native: failed granting role through service: {error}");
            return 1;
        }
    };

    println!(
        "granted role {} to client {}",
        session_role_name(response.role),
        response.client_id
    );
    0
}

fn run_permissions_revoke(context: &NativeCommandContext) -> i32 {
    let Some(session) = context.arguments.first() else {
        eprintln!("example.native permissions-revoke requires a session name or UUID");
        return 64;
    };

    let mut client_id = None;
    let mut args = context.arguments.iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--client" | "-c" => client_id = args.next().cloned(),
            other => {
                eprintln!("example.native permissions-revoke does not accept argument '{other}'");
                return 64;
            }
        }
    }

    let Some(client_id) = client_id else {
        eprintln!("example.native permissions-revoke requires --client <uuid>");
        return 64;
    };
    let client_id = match uuid::Uuid::parse_str(&client_id) {
        Ok(value) => value,
        Err(_) => {
            eprintln!("example.native permissions-revoke received invalid client id");
            return 64;
        }
    };

    let response = match context.call_service::<RevokePermissionRequest, RevokePermissionResponse>(
        "bmux.permissions.write",
        ServiceKind::Command,
        "permission-command/v1",
        "revoke",
        &RevokePermissionRequest {
            session: session.to_string(),
            client_id,
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("example.native: failed revoking role through service: {error}");
            return 1;
        }
    };

    println!("revoked explicit role for client {}", response.client_id);
    0
}

fn run_windows_list(context: &NativeCommandContext) -> i32 {
    let Some(session) = context.arguments.first() else {
        eprintln!("example.native windows-list requires a session name or UUID");
        return 64;
    };

    let response = match context.call_service::<ListWindowsRequest, ListWindowsResponse>(
        "bmux.windows.read",
        ServiceKind::Query,
        "window-query/v1",
        "list",
        &ListWindowsRequest {
            session: Some(session.to_string()),
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("example.native: failed listing windows through service: {error}");
            return 1;
        }
    };

    if response.windows.is_empty() {
        println!("example.native: no windows");
    } else {
        println!("example.native windows:");
        let mut table = Table::new(vec![
            TableColumn::new("ID").min_width(36),
            TableColumn::new("WINDOW"),
            TableColumn::new("ACTIVE"),
        ]);
        for window in response.windows {
            table.push_row(vec![
                window.id.to_string(),
                window.name.unwrap_or_else(|| format!("#{}", window.number)),
                if window.active {
                    "yes".to_string()
                } else {
                    "no".to_string()
                },
            ]);
        }
        if let Err(error) = write_stdout_table(&table) {
            eprintln!("example.native: failed rendering windows table: {error}");
            return 1;
        }
    }

    0
}

fn run_windows_new(context: &NativeCommandContext) -> i32 {
    let mut args = context.arguments.iter();
    let Some(session) = args.next() else {
        eprintln!("example.native windows-new requires a session name or UUID");
        return 64;
    };
    let mut name = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--name" | "-n" => name = args.next().cloned(),
            other => {
                eprintln!("example.native windows-new does not accept argument '{other}'");
                return 64;
            }
        }
    }

    let response = match context.call_service::<NewWindowRequest, NewWindowResponse>(
        "bmux.windows.write",
        ServiceKind::Command,
        "window-command/v1",
        "new",
        &NewWindowRequest {
            session: Some(session.to_string()),
            name,
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("example.native: failed creating window through service: {error}");
            return 1;
        }
    };

    println!(
        "created window: {} {}",
        response.window.id,
        response
            .window
            .name
            .unwrap_or_else(|| format!("#{}", response.window.number))
    );
    0
}

fn run_settings_show(context: &NativeCommandContext) -> i32 {
    let response = match context.call_service::<PluginSettingsRequest, PluginSettingsResponse>(
        "bmux.config.read",
        ServiceKind::Query,
        "config-query/v1",
        "plugin_settings",
        &PluginSettingsRequest {
            plugin_id: context.plugin_id.clone(),
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("example.native: failed reading settings through service: {error}");
            return 1;
        }
    };

    if response.settings.is_empty() {
        println!("example.native: no configured settings");
        return 0;
    }

    println!("example.native settings:");
    let mut table = Table::new(vec![TableColumn::new("SETTING")]);
    for (key, value) in response.settings {
        table.push_row(vec![format!("{key} = {value}")]);
    }
    if let Err(error) = write_stdout_table(&table) {
        eprintln!("example.native: failed rendering settings table: {error}");
        return 1;
    }
    0
}

fn run_storage_put(context: &NativeCommandContext) -> i32 {
    let Some(key) = context.arguments.first() else {
        eprintln!("example.native storage-put requires a key");
        return 64;
    };
    if context.arguments.len() < 2 {
        eprintln!("example.native storage-put requires a value");
        return 64;
    }
    let value = context.arguments[1..].join(" ").into_bytes();

    let result = context.call_service::<StorageSetRequest, ()>(
        "bmux.storage",
        ServiceKind::Command,
        "storage-command/v1",
        "set",
        &StorageSetRequest {
            key: key.to_string(),
            value,
        },
    );
    if let Err(error) = result {
        eprintln!("example.native: failed writing storage through service: {error}");
        return 1;
    }

    println!("stored key: {key}");
    0
}

fn run_storage_get(context: &NativeCommandContext) -> i32 {
    let Some(key) = context.arguments.first() else {
        eprintln!("example.native storage-get requires a key");
        return 64;
    };
    let response = match context.call_service::<StorageGetRequest, StorageGetResponse>(
        "bmux.storage",
        ServiceKind::Query,
        "storage-query/v1",
        "get",
        &StorageGetRequest {
            key: key.to_string(),
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("example.native: failed reading storage through service: {error}");
            return 1;
        }
    };

    match response.value {
        Some(value) => {
            let text = String::from_utf8_lossy(&value);
            println!("{key} = {text}");
        }
        None => println!("{key} is not set"),
    }
    0
}

fn write_stdout_table(table: &Table) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    write_table(&mut stdout, table)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListPermissionsRequest {
    session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListPermissionsResponse {
    permissions: Vec<bmux_ipc::SessionPermissionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GrantPermissionRequest {
    session: String,
    client_id: uuid::Uuid,
    role: bmux_ipc::SessionRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GrantPermissionResponse {
    client_id: uuid::Uuid,
    role: bmux_ipc::SessionRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RevokePermissionRequest {
    session: String,
    client_id: uuid::Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RevokePermissionResponse {
    client_id: uuid::Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListWindowsRequest {
    session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListWindowsResponse {
    windows: Vec<bmux_ipc::WindowSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NewWindowRequest {
    session: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NewWindowResponse {
    window: bmux_ipc::WindowSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PluginSettingsRequest {
    plugin_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PluginSettingsResponse {
    settings: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StorageSetRequest {
    key: String,
    value: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StorageGetRequest {
    key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StorageGetResponse {
    value: Option<Vec<u8>>,
}

fn session_role_name(role: bmux_ipc::SessionRole) -> &'static str {
    match role {
        bmux_ipc::SessionRole::Owner => "owner",
        bmux_ipc::SessionRole::Writer => "writer",
        bmux_ipc::SessionRole::Observer => "observer",
    }
}

fn parse_role(value: &str) -> Option<bmux_ipc::SessionRole> {
    match value {
        "owner" => Some(bmux_ipc::SessionRole::Owner),
        "writer" => Some(bmux_ipc::SessionRole::Writer),
        "observer" => Some(bmux_ipc::SessionRole::Observer),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::ExamplePlugin;
    use bmux_plugin::RustPlugin;

    #[test]
    fn descriptor_round_trips() {
        let descriptor = ExamplePlugin.descriptor();
        let serialized = descriptor
            .to_toml_string()
            .expect("descriptor should serialize");
        let reparsed = bmux_plugin::NativeDescriptor::from_toml_str(&serialized)
            .expect("descriptor should parse");
        assert_eq!(reparsed.id, "example.native");
        assert_eq!(reparsed.commands.len(), 9);
    }
}
