#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_cli_output::{Table, TableColumn, write_table};
use bmux_plugin::{
    EXIT_ERROR, EXIT_OK, EXIT_USAGE, NativeCommandContext, PluginEvent, RustPlugin, ServiceKind,
};
use serde::{Deserialize, Serialize};

#[derive(Default)]
struct ExamplePlugin;

impl RustPlugin for ExamplePlugin {
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
                EXIT_OK
            }
            _ => EXIT_USAGE,
        }
    }

    fn activate(&mut self, context: bmux_plugin::NativeLifecycleContext) -> i32 {
        println!("example.native: activated {}", context.plugin_id);
        EXIT_OK
    }

    fn deactivate(&mut self, context: bmux_plugin::NativeLifecycleContext) -> i32 {
        println!("example.native: deactivated {}", context.plugin_id);
        EXIT_OK
    }

    fn handle_event(&mut self, event: PluginEvent) -> i32 {
        println!("example.native: observed event {}", event.name);
        EXIT_OK
    }
}

bmux_plugin::export_plugin!(ExamplePlugin, include_str!("../plugin.toml"));

fn run_permissions_list(context: &NativeCommandContext) -> i32 {
    let Some(session) = context.arguments.first() else {
        eprintln!("example.native permissions-list requires a session name or UUID");
        return EXIT_USAGE;
    };

    let response = match context.call_service::<ListPermissionsRequest, ListPermissionsResponse>(
        "bmux.permissions.read",
        ServiceKind::Query,
        "permission-query/v1",
        "list",
        &ListPermissionsRequest {
            session: session.clone(),
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("example.native: failed listing permissions through service: {error}");
            return EXIT_ERROR;
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
            return EXIT_ERROR;
        }
    }

    EXIT_OK
}

fn run_permissions_grant(context: &NativeCommandContext) -> i32 {
    let Some(session) = context.arguments.first() else {
        eprintln!("example.native permissions-grant requires a session name or UUID");
        return EXIT_USAGE;
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
                return EXIT_USAGE;
            }
        }
    }

    let Some(client_id) = client_id else {
        eprintln!("example.native permissions-grant requires --client <uuid>");
        return EXIT_USAGE;
    };
    let client_id = if let Ok(value) = uuid::Uuid::parse_str(&client_id) {
        value
    } else {
        eprintln!("example.native permissions-grant received invalid client id");
        return EXIT_USAGE;
    };
    let Some(role) = role else {
        eprintln!("example.native permissions-grant requires --role <role>");
        return EXIT_USAGE;
    };
    let Some(role) = parse_role(&role) else {
        eprintln!("example.native permissions-grant received invalid role '{role}'");
        return EXIT_USAGE;
    };

    let response = match context.call_service::<GrantPermissionRequest, GrantPermissionResponse>(
        "bmux.permissions.write",
        ServiceKind::Command,
        "permission-command/v1",
        "grant",
        &GrantPermissionRequest {
            session: session.clone(),
            client_id,
            role,
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("example.native: failed granting role through service: {error}");
            return EXIT_ERROR;
        }
    };

    println!(
        "granted role {} to client {}",
        session_role_name(response.role),
        response.client_id
    );
    EXIT_OK
}

fn run_permissions_revoke(context: &NativeCommandContext) -> i32 {
    let Some(session) = context.arguments.first() else {
        eprintln!("example.native permissions-revoke requires a session name or UUID");
        return EXIT_USAGE;
    };

    let mut client_id = None;
    let mut args = context.arguments.iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--client" | "-c" => client_id = args.next().cloned(),
            other => {
                eprintln!("example.native permissions-revoke does not accept argument '{other}'");
                return EXIT_USAGE;
            }
        }
    }

    let Some(client_id) = client_id else {
        eprintln!("example.native permissions-revoke requires --client <uuid>");
        return EXIT_USAGE;
    };
    let client_id = if let Ok(value) = uuid::Uuid::parse_str(&client_id) {
        value
    } else {
        eprintln!("example.native permissions-revoke received invalid client id");
        return EXIT_USAGE;
    };

    let response = match context.call_service::<RevokePermissionRequest, RevokePermissionResponse>(
        "bmux.permissions.write",
        ServiceKind::Command,
        "permission-command/v1",
        "revoke",
        &RevokePermissionRequest {
            session: session.clone(),
            client_id,
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("example.native: failed revoking role through service: {error}");
            return EXIT_ERROR;
        }
    };

    println!("revoked explicit role for client {}", response.client_id);
    EXIT_OK
}

fn run_windows_list(context: &NativeCommandContext) -> i32 {
    let Some(session) = context.arguments.first() else {
        eprintln!("example.native windows-list requires a session name or UUID");
        return EXIT_USAGE;
    };

    let response = match context.call_service::<ListWindowsRequest, ListWindowsResponse>(
        "bmux.windows.read",
        ServiceKind::Query,
        "window-query/v1",
        "list",
        &ListWindowsRequest {
            session: Some(session.clone()),
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("example.native: failed listing windows through service: {error}");
            return EXIT_ERROR;
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
            return EXIT_ERROR;
        }
    }

    EXIT_OK
}

fn run_windows_new(context: &NativeCommandContext) -> i32 {
    let mut args = context.arguments.iter();
    let Some(session) = args.next() else {
        eprintln!("example.native windows-new requires a session name or UUID");
        return EXIT_USAGE;
    };
    let mut name = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--name" | "-n" => name = args.next().cloned(),
            other => {
                eprintln!("example.native windows-new does not accept argument '{other}'");
                return EXIT_USAGE;
            }
        }
    }

    let response = match context.call_service::<NewWindowRequest, NewWindowResponse>(
        "bmux.windows.write",
        ServiceKind::Command,
        "window-command/v1",
        "new",
        &NewWindowRequest {
            session: Some(session.clone()),
            name,
        },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("example.native: failed creating window through service: {error}");
            return EXIT_ERROR;
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
    EXIT_OK
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
            return EXIT_ERROR;
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
        return EXIT_ERROR;
    }
    EXIT_OK
}

fn run_storage_put(context: &NativeCommandContext) -> i32 {
    let Some(key) = context.arguments.first() else {
        eprintln!("example.native storage-put requires a key");
        return EXIT_USAGE;
    };
    if context.arguments.len() < 2 {
        eprintln!("example.native storage-put requires a value");
        return EXIT_USAGE;
    }
    let value = context.arguments[1..].join(" ").into_bytes();

    let result = context.call_service::<StorageSetRequest, ()>(
        "bmux.storage",
        ServiceKind::Command,
        "storage-command/v1",
        "set",
        &StorageSetRequest {
            key: key.clone(),
            value,
        },
    );
    if let Err(error) = result {
        eprintln!("example.native: failed writing storage through service: {error}");
        return EXIT_ERROR;
    }

    println!("stored key: {key}");
    EXIT_OK
}

fn run_storage_get(context: &NativeCommandContext) -> i32 {
    let Some(key) = context.arguments.first() else {
        eprintln!("example.native storage-get requires a key");
        return EXIT_USAGE;
    };
    let response = match context.call_service::<StorageGetRequest, StorageGetResponse>(
        "bmux.storage",
        ServiceKind::Query,
        "storage-query/v1",
        "get",
        &StorageGetRequest { key: key.clone() },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("example.native: failed reading storage through service: {error}");
            return EXIT_ERROR;
        }
    };

    match response.value {
        Some(value) => {
            let text = String::from_utf8_lossy(&value);
            println!("{key} = {text}");
        }
        None => println!("{key} is not set"),
    }
    EXIT_OK
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
    permissions: Vec<PermissionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PermissionSummary {
    client_id: uuid::Uuid,
    role: SessionRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SessionRole {
    Owner,
    Writer,
    Observer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GrantPermissionRequest {
    session: String,
    client_id: uuid::Uuid,
    role: SessionRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GrantPermissionResponse {
    client_id: uuid::Uuid,
    role: SessionRole,
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
    windows: Vec<WorkspaceWindowSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NewWindowRequest {
    session: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NewWindowResponse {
    window: WorkspaceWindowSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WorkspaceWindowSummary {
    id: uuid::Uuid,
    session_id: uuid::Uuid,
    number: u32,
    name: Option<String>,
    active: bool,
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

const fn session_role_name(role: SessionRole) -> &'static str {
    match role {
        SessionRole::Owner => "owner",
        SessionRole::Writer => "writer",
        SessionRole::Observer => "observer",
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

#[cfg(test)]
mod tests {
    #[test]
    fn manifest_parses_and_validates() {
        let manifest = bmux_plugin::PluginManifest::from_toml_str(include_str!("../plugin.toml"))
            .expect("manifest should parse");
        assert_eq!(manifest.id, "example.native");
        assert_eq!(manifest.commands.len(), 9);
        let declaration = manifest.to_declaration().expect("manifest should validate");
        assert_eq!(declaration.id.as_str(), "example.native");
    }
}
