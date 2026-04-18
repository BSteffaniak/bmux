#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![cfg_attr(feature = "static-bundled", allow(dead_code))]

use bmux_cli::attach::{
    self as attach_prompt, PromptOption, PromptRequest, PromptResponse, PromptSubmitError,
    PromptValue,
};
use bmux_cli_output::{Table, TableColumn, write_table};
use bmux_plugin::ServiceCaller;
use bmux_plugin_sdk::EXIT_USAGE;
use bmux_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Default)]
struct ExamplePlugin;

const PROMPT_HOST_WAIT_TIMEOUT: Duration = Duration::from_secs(8);
const PROMPT_HOST_WAIT_POLL: Duration = Duration::from_millis(75);

impl RustPlugin for ExamplePlugin {
    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        bmux_plugin_sdk::route_command!(context, {
            "permissions-list" => Ok(run_permissions_list(&context)),
            "permissions-grant" => Ok(run_permissions_grant(&context)),
            "permissions-revoke" => Ok(run_permissions_revoke(&context)),
            "windows-list" => Ok(run_windows_list(&context)),
            "windows-new" => Ok(run_windows_new(&context)),
            "settings-show" => Ok(run_settings_show(&context)),
            "storage-put" => Ok(run_storage_put(&context)),
            "storage-get" => Ok(run_storage_get(&context)),
            "prompt-showcase" => Ok(run_prompt_showcase_command()),
            "hello" => {
                if context.arguments.is_empty() {
                    println!("example.native: hello from bmux plugin");
                } else {
                    println!("example.native: hello {}", context.arguments.join(" "));
                }
                Ok(EXIT_OK)
            },
        })
    }

    fn activate(
        &mut self,
        context: bmux_plugin_sdk::NativeLifecycleContext,
    ) -> Result<i32, PluginCommandError> {
        println!("example.native: activated {}", context.plugin_id);
        Ok(EXIT_OK)
    }

    fn deactivate(
        &mut self,
        context: bmux_plugin_sdk::NativeLifecycleContext,
    ) -> Result<i32, PluginCommandError> {
        println!("example.native: deactivated {}", context.plugin_id);
        Ok(EXIT_OK)
    }

    fn handle_event(&mut self, event: PluginEvent) -> Result<i32, PluginCommandError> {
        println!("example.native: observed event {}", event.name);
        Ok(EXIT_OK)
    }
}

bmux_plugin_sdk::export_plugin!(ExamplePlugin, include_str!("../plugin.toml"));

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
    let Ok(client_id) = uuid::Uuid::parse_str(&client_id) else {
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
    let Ok(client_id) = uuid::Uuid::parse_str(&client_id) else {
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

    let windows = match context.call_service::<ListWindowsRequest, Vec<WindowEntry>>(
        "bmux.windows.read",
        ServiceKind::Query,
        "windows-state",
        "list-windows",
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

    if windows.is_empty() {
        println!("example.native: no windows");
    } else {
        println!("example.native windows:");
        let mut table = Table::new(vec![
            TableColumn::new("ID").min_width(36),
            TableColumn::new("WINDOW"),
            TableColumn::new("ACTIVE"),
        ]);
        for window in windows {
            table.push_row(vec![
                window.id,
                window.name,
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

    let _ = session; // No longer scoped per-session in the typed command.
    let ack = match context.call_service::<NewWindowRequest, WindowAck>(
        "bmux.windows.write",
        ServiceKind::Command,
        "windows-commands",
        "new-window",
        &NewWindowRequest { name },
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("example.native: failed creating window through service: {error}");
            return EXIT_ERROR;
        }
    };

    println!(
        "created window: ok={} id={}",
        ack.ok,
        ack.id.as_deref().unwrap_or("(none)"),
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

fn run_prompt_showcase_command() -> i32 {
    spawn_prompt_showcase_task();
    println!("example.native: prompt showcase task started");
    EXIT_OK
}

fn spawn_prompt_showcase_task() {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        std::mem::drop(handle.spawn(async {
            print_prompt_showcase_result(run_prompt_showcase_sequence().await);
        }));
        return;
    }

    std::mem::drop(std::thread::spawn(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build();
        match runtime {
            Ok(runtime) => {
                let result = runtime.block_on(run_prompt_showcase_sequence());
                print_prompt_showcase_result(result);
            }
            Err(error) => {
                eprintln!("example.native: failed starting prompt showcase runtime: {error}");
            }
        }
    }));
}

/// Run the plugin-provided prompt showcase sequence.
///
/// # Errors
/// Returns an error string when the attach prompt host is unavailable,
/// disconnects, or prompt delivery fails.
pub async fn run_prompt_showcase_sequence() -> std::result::Result<Vec<String>, String> {
    let mut lines = Vec::new();

    let confirm = request_prompt_with_retry(|| {
        PromptRequest::confirm("Plugin Prompt Showcase")
            .message("These prompts are requested from example.native plugin code.")
            .submit_label("Continue")
            .cancel_label("Stop")
            .confirm_default(true)
            .confirm_labels("Continue", "Stop")
    })
    .await?;
    lines.push(format_prompt_result_line("confirm", &confirm));

    if !prompt_response_is_confirmed(&confirm) {
        lines.push("showcase cancelled before running remaining prompt types".to_string());
        return Ok(lines);
    }

    let text_input = request_prompt_with_retry(|| {
        PromptRequest::text_input("Plugin Label")
            .message("Enter a short label for this plugin-driven prompt run.")
            .input_placeholder("plugin-demo")
            .input_required(true)
            .submit_label("Save")
            .cancel_label("Skip")
    })
    .await?;
    lines.push(format_prompt_result_line("text_input", &text_input));

    let single_select = request_prompt_with_retry(|| {
        PromptRequest::single_select(
            "Plugin Theme",
            vec![
                PromptOption::new("classic", "Classic"),
                PromptOption::new("compact", "Compact"),
                PromptOption::new("focus", "Focus"),
            ],
        )
        .message("Pick a display mode for this plugin demo.")
        .single_default_index(0)
        .submit_label("Select")
        .cancel_label("Skip")
    })
    .await?;
    lines.push(format_prompt_result_line("single_select", &single_select));

    let multi_toggle = request_prompt_with_retry(|| {
        PromptRequest::multi_toggle(
            "Plugin Flags",
            vec![
                PromptOption::new("clipboard", "Clipboard sync"),
                PromptOption::new("activity", "Activity hints"),
                PromptOption::new("notifications", "Notifications"),
                PromptOption::new("autofocus", "Auto focus new panes"),
            ],
        )
        .message("Toggle one or more plugin-managed flags.")
        .multi_defaults(vec![0, 1])
        .multi_min_selected(1)
        .submit_label("Apply")
        .cancel_label("Skip")
    })
    .await?;
    lines.push(format_prompt_result_line("multi_toggle", &multi_toggle));

    let done = request_prompt_with_retry(|| {
        PromptRequest::confirm("Plugin Showcase Complete")
            .message("Plugin-provided prompts are complete. Press Ctrl+b then d to detach.")
            .submit_label("Done")
            .cancel_label("Repeat")
            .confirm_default(true)
            .confirm_labels("Done", "Repeat")
    })
    .await?;
    lines.push(format_prompt_result_line("completion", &done));

    Ok(lines)
}

async fn request_prompt_with_retry<F>(build: F) -> std::result::Result<PromptResponse, String>
where
    F: Fn() -> PromptRequest,
{
    let started = Instant::now();

    loop {
        match attach_prompt::request_prompt(build()).await {
            Ok(response) => return Ok(response),
            Err(PromptSubmitError::HostUnavailable)
                if started.elapsed() < PROMPT_HOST_WAIT_TIMEOUT =>
            {
                tokio::time::sleep(PROMPT_HOST_WAIT_POLL).await;
            }
            Err(PromptSubmitError::HostUnavailable) => {
                return Err("prompt host did not become available in time".to_string());
            }
            Err(PromptSubmitError::HostDisconnected) => {
                return Err("prompt host disconnected while running plugin showcase".to_string());
            }
        }
    }
}

const fn prompt_response_is_confirmed(response: &PromptResponse) -> bool {
    matches!(
        response,
        PromptResponse::Submitted(PromptValue::Confirm(true))
    )
}

fn format_prompt_result_line(label: &str, response: &PromptResponse) -> String {
    let value = match response {
        PromptResponse::Submitted(PromptValue::Confirm(value)) => format!("confirm={value}"),
        PromptResponse::Submitted(PromptValue::Text(value)) => format!("text={value}"),
        PromptResponse::Submitted(PromptValue::Single(value)) => format!("single={value}"),
        PromptResponse::Submitted(PromptValue::Multi(values)) => {
            format!("multi={}", values.join(", "))
        }
        PromptResponse::Cancelled => "cancelled".to_string(),
        PromptResponse::RejectedBusy => "rejected_busy".to_string(),
    };
    format!("{label}: {value}")
}

fn print_prompt_showcase_result(result: std::result::Result<Vec<String>, String>) {
    match result {
        Ok(lines) => {
            println!("example.native: prompt showcase completed");
            for line in lines {
                println!("example.native: {line}");
            }
        }
        Err(error) => {
            eprintln!("example.native: prompt showcase failed: {error}");
        }
    }
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
struct NewWindowRequest {
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WindowAck {
    ok: bool,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WindowEntry {
    id: String,
    name: String,
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
        assert_eq!(manifest.commands.len(), 10);
        let declaration = manifest.to_declaration().expect("manifest should validate");
        assert_eq!(declaration.id.as_str(), "example.native");
    }
}
