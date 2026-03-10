#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

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
            required_capabilities: BTreeSet::from([
                HostScope::new("bmux.commands").expect("host scope should parse"),
                HostScope::new("bmux.events.subscribe").expect("host scope should parse"),
                HostScope::new("bmux.permissions.read").expect("host scope should parse"),
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
            ],
            event_subscriptions: vec![PluginEventSubscription {
                kinds: BTreeSet::from([PluginEventKind::System, PluginEventKind::Window]),
                names: BTreeSet::from(["server_started".to_string(), "window_created".to_string()]),
            }],
            dependencies: vec![bmux_plugin::PluginDependency {
                plugin_id: bmux_plugin::PluginId::new("bmux.permissions")
                    .expect("plugin id should parse"),
                version_req: format!("={}", env!("CARGO_PKG_VERSION")),
                required: true,
            }],
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
        for permission in response.permissions {
            println!(
                "{} {}",
                permission.client_id,
                session_role_name(permission.role)
            );
        }
    }

    0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListPermissionsRequest {
    session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListPermissionsResponse {
    permissions: Vec<bmux_ipc::SessionPermissionSummary>,
}

fn session_role_name(role: bmux_ipc::SessionRole) -> &'static str {
    match role {
        bmux_ipc::SessionRole::Owner => "owner",
        bmux_ipc::SessionRole::Writer => "writer",
        bmux_ipc::SessionRole::Observer => "observer",
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
        assert_eq!(reparsed.commands.len(), 2);
    }
}
