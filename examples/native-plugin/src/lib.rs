#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_client::BmuxClient;
use bmux_config::ConfigPaths;
use bmux_ipc::SessionSelector;
use bmux_plugin::{
    CommandExecutionKind, NativeCommandContext, NativeDescriptor, PluginCapability, PluginCommand,
    PluginEvent, PluginEventKind, PluginEventSubscription, RustPlugin,
};
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
            capabilities: [
                PluginCapability::Commands,
                PluginCapability::EventSubscription,
            ]
            .into_iter()
            .collect(),
            commands: vec![
                PluginCommand {
                    name: "hello".to_string(),
                    path: Vec::new(),
                    aliases: Vec::new(),
                    summary: "Print a hello message".to_string(),
                    description: None,
                    arguments: Vec::new(),
                    execution: CommandExecutionKind::HostCallback,
                    expose_in_cli: true,
                },
                PluginCommand {
                    name: "permissions-list".to_string(),
                    path: Vec::new(),
                    aliases: Vec::new(),
                    summary: "List session permissions through bmux host IPC".to_string(),
                    description: None,
                    arguments: Vec::new(),
                    execution: CommandExecutionKind::HostCallback,
                    expose_in_cli: true,
                },
            ],
            event_subscriptions: vec![PluginEventSubscription {
                kinds: BTreeSet::from([PluginEventKind::System, PluginEventKind::Window]),
                names: BTreeSet::from(["server_started".to_string(), "window_created".to_string()]),
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

    let paths = ConfigPaths::new(
        context.connection.config_dir.clone().into(),
        context.connection.runtime_dir.clone().into(),
        context.connection.data_dir.clone().into(),
    );

    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            tokio::task::block_in_place(|| handle.block_on(async_permissions_list(&paths, session)))
        }
        Err(_) => match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(async_permissions_list(&paths, session)),
            Err(_) => 70,
        },
    }
}

async fn async_permissions_list(paths: &ConfigPaths, session: &str) -> i32 {
    let selector = parse_session_selector(session);
    match BmuxClient::connect_with_paths(paths, "example-native-plugin").await {
        Ok(mut client) => match client.list_permissions(selector).await {
            Ok(permissions) => {
                if permissions.is_empty() {
                    println!("example.native: no explicit role assignments");
                } else {
                    println!("example.native permissions:");
                    for permission in permissions {
                        println!(
                            "{} {}",
                            permission.client_id,
                            session_role_name(permission.role)
                        );
                    }
                }
                0
            }
            Err(error) => {
                eprintln!("example.native: failed listing permissions: {error}");
                1
            }
        },
        Err(error) => {
            eprintln!("example.native: failed connecting to bmux host: {error}");
            1
        }
    }
}

fn parse_session_selector(value: &str) -> SessionSelector {
    match uuid::Uuid::parse_str(value) {
        Ok(id) => SessionSelector::ById(id),
        Err(_) => SessionSelector::ByName(value.to_string()),
    }
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
