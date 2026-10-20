//! Optional plugin-owned presentation commands and authoritative managed hosts.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
mod broadcast;
mod cli;
mod commands;
mod demo;
mod gallery;
mod managed_input;
mod mirror;
mod owned_runtime;
mod owner;
pub mod playbook;
pub mod playbook_capture;
pub mod playbook_component;
pub mod playbook_controller;
mod playbook_live;
mod playbook_observer;
mod player;
mod player_evidence;
mod rich;
mod services;
mod source;
mod tls;
mod viewer;
mod web;
mod web_host;

pub use demo::linked_registry as linked_demo_registry;
pub use owner::LinkedOwner;

use bmux_plugin_sdk::{
    NativeCommandContext, NativeCommandInvocationSource, NativeServiceContext, PluginCommandError,
    RustPlugin, ServiceResponse,
};

#[derive(Default)]
pub struct PresenterPlugin;
impl RustPlugin for PresenterPlugin {
    type Contract = bmux_presenter_plugin_api::Contract;
    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        if context.invocation_source != NativeCommandInvocationSource::Cli {
            return Err(PluginCommandError::failed(
                "present commands require standalone CLI invocation; attach terminal is already owned",
            ));
        }
        let command = cli::Command::parse(&context.command, &context.arguments)
            .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| PluginCommandError::failed(error.to_string()))?;
                    runtime
                        .block_on(commands::run(command, &context))
                        .map(|()| bmux_plugin_sdk::EXIT_OK)
                        .map_err(|error| PluginCommandError::failed(format!("{error:#}")))
                })
                .join()
                .unwrap_or_else(|_| {
                    Err(PluginCommandError::failed(
                        "presentation command panicked; terminal cleanup attempted",
                    ))
                })
        })
    }
    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        services::invoke(context)
    }
}

bmux_plugin_sdk::export_plugin!(PresenterPlugin, include_str!("../plugin.toml"));

#[cfg(test)]
mod tests {
    #[test]
    fn manifest_declares_packaged_mermaid_and_caller_owned_commands() {
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(include_str!("../plugin.toml")).unwrap();
        assert!(manifest.auxiliary_executables.is_empty());
        assert_eq!(manifest.commands.len(), 4);
        assert!(
            manifest
                .commands
                .iter()
                .all(|command| command.path.len() == 2 && command.path[0] == "present")
        );
        for name in [
            "present-play",
            "present-host",
            "present-join",
            "present-check",
        ] {
            let command = manifest
                .commands
                .iter()
                .find(|command| command.name == name)
                .unwrap();
            assert_eq!(
                command.execution,
                bmux_plugin_sdk::CommandExecutionKind::CallerProcess
            );
        }
    }
}
