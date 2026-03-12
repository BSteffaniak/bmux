#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin::{
    CommandExecutionKind, HostRuntimeApi, HostScope, NativeCommandContext, NativeDescriptor,
    NativeServiceContext, PaneListRequest, PluginCommand, PluginCommandArgument,
    PluginCommandArgumentKind, PluginService, RustPlugin, ServiceKind, ServiceResponse,
    SessionCreateRequest, SessionKillRequest, SessionSelector, decode_service_message,
    encode_service_message,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Default)]
struct WindowsPlugin;

impl RustPlugin for WindowsPlugin {
    fn descriptor(&self) -> NativeDescriptor {
        NativeDescriptor::builder("bmux.windows", "bmux Windows")
            .plugin_version(env!("CARGO_PKG_VERSION"))
            .description("Shipped bmux windows command plugin")
            .require_capability("bmux.commands")
            .expect("capability should parse")
            .provide_capability("bmux.windows.read")
            .expect("capability should parse")
            .provide_capability("bmux.windows.write")
            .expect("capability should parse")
            .provide_feature("bmux.windows")
            .expect("feature should parse")
            .service(PluginService {
                capability: HostScope::new("bmux.windows.read").expect("host scope should parse"),
                kind: ServiceKind::Query,
                interface_id: "window-query/v1".to_string(),
            })
            .service(PluginService {
                capability: HostScope::new("bmux.windows.write").expect("host scope should parse"),
                kind: ServiceKind::Command,
                interface_id: "window-command/v1".to_string(),
            })
            .command(plugin_command(
                "new-window",
                "Create a workspace window",
                vec![vec!["window", "new"]],
            ))
            .command(plugin_command(
                "list-windows",
                "List workspace windows",
                vec![vec!["window", "list"]],
            ))
            .command(plugin_command(
                "kill-window",
                "Kill a workspace window",
                vec![vec!["window", "kill"]],
            ))
            .command(plugin_command(
                "kill-all-windows",
                "Kill all workspace windows",
                vec![vec!["window", "kill-all"]],
            ))
            .command(plugin_command(
                "switch-window",
                "Switch active workspace window",
                vec![vec!["window", "switch"]],
            ))
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
            ("window-query/v1", "list") => {
                let request =
                    match decode_service_message::<ListWindowsRequest>(&context.request.payload) {
                        Ok(request) => request,
                        Err(error) => {
                            return ServiceResponse::error("invalid_request", error.to_string());
                        }
                    };
                let windows = match list_windows(&context, request.session.as_deref()) {
                    Ok(windows) => windows,
                    Err(error) => {
                        return ServiceResponse::error("list_failed", error.to_string());
                    }
                };
                let payload = match encode_service_message(&ListWindowsResponse { windows }) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            ("window-command/v1", "new") => {
                let request =
                    match decode_service_message::<NewWindowRequest>(&context.request.payload) {
                        Ok(request) => request,
                        Err(error) => {
                            return ServiceResponse::error("invalid_request", error.to_string());
                        }
                    };
                let response = match context
                    .session_create(&SessionCreateRequest { name: request.name })
                {
                    Ok(response) => response,
                    Err(error) => return ServiceResponse::error("new_failed", error.to_string()),
                };
                let payload = match encode_service_message(&WindowCommandAck {
                    ok: true,
                    id: Some(response.id.to_string()),
                }) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            ("window-command/v1", "kill") => {
                let request =
                    match decode_service_message::<KillWindowRequest>(&context.request.payload) {
                        Ok(request) => request,
                        Err(error) => {
                            return ServiceResponse::error("invalid_request", error.to_string());
                        }
                    };
                let selector = match parse_selector(&request.target) {
                    Ok(selector) => selector,
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error.to_string());
                    }
                };
                let response = match context.session_kill(&SessionKillRequest {
                    selector,
                    force_local: request.force_local,
                }) {
                    Ok(response) => response,
                    Err(error) => return ServiceResponse::error("kill_failed", error.to_string()),
                };
                let payload = match encode_service_message(&WindowCommandAck {
                    ok: true,
                    id: Some(response.id.to_string()),
                }) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            ("window-command/v1", "kill_all") => {
                let request =
                    match decode_service_message::<KillAllWindowsRequest>(&context.request.payload)
                    {
                        Ok(request) => request,
                        Err(error) => {
                            return ServiceResponse::error("invalid_request", error.to_string());
                        }
                    };
                let sessions = match context.session_list() {
                    Ok(response) => response.sessions,
                    Err(error) => return ServiceResponse::error("kill_failed", error.to_string()),
                };
                for session in sessions {
                    if let Err(error) = context.session_kill(&SessionKillRequest {
                        selector: SessionSelector::ById(session.id),
                        force_local: request.force_local,
                    }) {
                        return ServiceResponse::error("kill_failed", error.to_string());
                    }
                }
                let payload = match encode_service_message(&WindowCommandAck { ok: true, id: None })
                {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            ("window-command/v1", "switch") => {
                let payload = match encode_service_message(&WindowCommandAck { ok: true, id: None })
                {
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
                    "unsupported windows service invocation '{}:{}'",
                    context.request.service.interface_id, context.request.operation,
                ),
            ),
        }
    }
}

fn handle_command(context: &NativeCommandContext) -> Result<(), String> {
    match context.command.as_str() {
        "new-window" => {
            let name = option_value(&context.arguments, "name");
            let response = context
                .session_create(&SessionCreateRequest { name })
                .map_err(|error| error.to_string())?;
            println!("created window session: {}", response.id);
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
                .session_kill(&SessionKillRequest {
                    selector,
                    force_local,
                })
                .map_err(|error| error.to_string())?;
            println!("killed window session: {}", response.id);
            Ok(())
        }
        "kill-all-windows" => {
            let force_local = has_flag(&context.arguments, "force-local");
            let sessions = context
                .session_list()
                .map_err(|error| error.to_string())?
                .sessions;
            if sessions.is_empty() {
                println!("no windows");
                return Ok(());
            }
            for session in sessions {
                let response = context
                    .session_kill(&SessionKillRequest {
                        selector: SessionSelector::ById(session.id),
                        force_local,
                    })
                    .map_err(|error| error.to_string())?;
                println!("killed window session: {}", response.id);
            }
            Ok(())
        }
        "switch-window" => {
            let target = positional_value(&context.arguments)
                .ok_or_else(|| "missing required TARGET argument".to_string())?;
            let selector = parse_selector(&target)?;
            let session_id = resolve_session_id(context, selector)?;
            let pane_response = context
                .pane_list(&PaneListRequest {
                    session: Some(SessionSelector::ById(session_id)),
                })
                .map_err(|error| error.to_string())?;
            if pane_response.panes.is_empty() {
                return Err("target window has no panes".to_string());
            }
            println!(
                "switch-window selected session {} (attach command will enter it)",
                session_id
            );
            Ok(())
        }
        _ => Err(format!("unsupported command '{}'", context.command)),
    }
}

fn list_windows(
    caller: &impl HostRuntimeApi,
    session_filter: Option<&str>,
) -> Result<Vec<WindowEntry>, String> {
    let sessions = caller
        .session_list()
        .map_err(|error| error.to_string())?
        .sessions;
    let selected = if let Some(filter) = session_filter {
        let selector = parse_selector(filter)?;
        sessions
            .into_iter()
            .filter(|session| match &selector {
                SessionSelector::ById(id) => &session.id == id,
                SessionSelector::ByName(name) => session.name.as_deref() == Some(name.as_str()),
            })
            .collect::<Vec<_>>()
    } else {
        sessions
    };

    Ok(selected
        .into_iter()
        .enumerate()
        .map(|(index, session)| WindowEntry {
            id: session.id.to_string(),
            name: session
                .name
                .unwrap_or_else(|| format!("session-{}", index.saturating_add(1))),
            active: index == 0,
        })
        .collect())
}

fn resolve_session_id(
    caller: &impl HostRuntimeApi,
    selector: SessionSelector,
) -> Result<Uuid, String> {
    let sessions = caller
        .session_list()
        .map_err(|error| error.to_string())?
        .sessions;
    let session = sessions.into_iter().find(|session| match &selector {
        SessionSelector::ById(id) => session.id == *id,
        SessionSelector::ByName(name) => session.name.as_deref() == Some(name.as_str()),
    });
    session
        .map(|session| session.id)
        .ok_or_else(|| "target session not found".to_string())
}

fn parse_selector(value: &str) -> Result<SessionSelector, String> {
    if let Ok(id) = Uuid::parse_str(value) {
        return Ok(SessionSelector::ById(id));
    }
    if value.trim().is_empty() {
        return Err("target must not be empty".to_string());
    }
    Ok(SessionSelector::ByName(value.to_string()))
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

fn plugin_command(name: &str, summary: &str, aliases: Vec<Vec<&str>>) -> PluginCommand {
    let mut command = PluginCommand::new(name, summary)
        .path([name])
        .execution(CommandExecutionKind::ProviderExec)
        .expose_in_cli(true);
    for alias in aliases {
        command = command.alias(alias);
    }
    for argument in command_arguments(name) {
        command = command.argument(argument);
    }
    command
}

fn command_arguments(name: &str) -> Vec<PluginCommandArgument> {
    match name {
        "new-window" => vec![
            PluginCommandArgument::option("session", PluginCommandArgumentKind::String).short('s'),
            PluginCommandArgument::option("name", PluginCommandArgumentKind::String).short('n'),
        ],
        "list-windows" => vec![
            PluginCommandArgument::option("session", PluginCommandArgumentKind::String).short('s'),
            PluginCommandArgument::flag("json").short('j'),
        ],
        "kill-window" => vec![
            PluginCommandArgument::positional("target", PluginCommandArgumentKind::String)
                .required(true),
            PluginCommandArgument::option("session", PluginCommandArgumentKind::String).short('s'),
            PluginCommandArgument::flag("force-local"),
        ],
        "kill-all-windows" => vec![
            PluginCommandArgument::option("session", PluginCommandArgumentKind::String).short('s'),
            PluginCommandArgument::flag("force-local"),
        ],
        "switch-window" => vec![
            PluginCommandArgument::positional("target", PluginCommandArgumentKind::String)
                .required(true),
            PluginCommandArgument::option("session", PluginCommandArgumentKind::String).short('s'),
        ],
        _ => Vec::new(),
    }
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

bmux_plugin::export_plugin!(WindowsPlugin);
