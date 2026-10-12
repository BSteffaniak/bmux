//! Optional plugin-owned presentation commands and authoritative managed hosts.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
mod cli;
mod commands;
mod demo;
mod mirror;
mod owner;
mod player;
mod rich;
mod services;
mod source;
mod tls;
mod viewer;

use bmux_plugin_sdk::{
    NativeCommandContext, NativeCommandInvocationSource, NativeServiceContext, PluginCommandError,
    RustPlugin, ServiceResponse,
};

#[derive(Default)]
pub struct PresentationPlugin;
impl RustPlugin for PresentationPlugin {
    type Contract = bmux_presentation_plugin_api::Contract;
    fn activate(
        &mut self,
        context: bmux_plugin_sdk::NativeLifecycleContext,
    ) -> Result<i32, PluginCommandError> {
        source::activate(&context)
            .map(|()| bmux_plugin_sdk::EXIT_OK)
            .map_err(|error| PluginCommandError::failed(error.to_string()))
    }
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

bmux_plugin_sdk::export_plugin!(PresentationPlugin, include_str!("../plugin.toml"));

#[cfg(test)]
mod tests {
    #[test]
    fn manifest_declares_packaged_mermaid_and_caller_owned_commands() {
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(include_str!("../plugin.toml")).unwrap();
        let helper = manifest
            .auxiliary_executables
            .iter()
            .find(|helper| helper.id == "mermaid-worker")
            .unwrap();
        assert_eq!(helper.cargo_package, "bmux_mermaid_render");
        assert_eq!(helper.cargo_binary, "bmux-mermaid-worker");
        assert_eq!(helper.relative_path, "bmux-mermaid-worker");
        assert_eq!(helper.protocol_version, 1);
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
