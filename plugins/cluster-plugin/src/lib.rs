#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bmux_plugin_sdk::prelude::*;

#[derive(Default)]
pub struct ClusterPlugin;

impl RustPlugin for ClusterPlugin {
    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        if is_supported_command(context.command.as_str()) {
            return Err(PluginCommandError::from(format!(
                "command '{}' is not implemented yet",
                context.command
            )));
        }

        Err(PluginCommandError::from(format!(
            "unsupported command '{}'",
            context.command
        )))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        ServiceResponse::error(
            "not_implemented",
            format!(
                "service {}:{} is not implemented yet",
                context.request.service.interface_id, context.request.operation
            ),
        )
    }
}

fn is_supported_command(command: &str) -> bool {
    matches!(
        command,
        "cluster-up"
            | "cluster-status"
            | "cluster-doctor"
            | "cluster-hosts"
            | "cluster-pane-new"
            | "cluster-pane-move"
            | "cluster-pane-retry"
    )
}
