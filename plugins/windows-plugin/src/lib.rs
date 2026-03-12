#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin::{
    CommandExecutionKind, HostScope, NativeCommandContext, NativeDescriptor, NativeServiceContext,
    PluginCommand, PluginCommandArgument, PluginCommandArgumentKind, PluginService, RustPlugin,
    ServiceKind, ServiceResponse, decode_service_message, encode_service_message,
};
use serde::{Deserialize, Serialize};

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
        let _ = context;
        println!("windows provider active (core baseline is single terminal)");
        0
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
                let _ = request;
                let payload = match encode_service_message(&ListWindowsResponse {
                    windows: vec![WindowEntry {
                        id: "terminal".to_string(),
                        name: "terminal".to_string(),
                        active: true,
                    }],
                }) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return ServiceResponse::error("encode_failed", error.to_string());
                    }
                };
                ServiceResponse::ok(payload)
            }
            ("window-command/v1", "new")
            | ("window-command/v1", "switch")
            | ("window-command/v1", "kill")
            | ("window-command/v1", "kill_all") => {
                let payload = match encode_service_message(&WindowCommandAck { ok: true }) {
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
}

bmux_plugin::export_plugin!(WindowsPlugin);
