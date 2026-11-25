//! `bmux drive`: agent-facing terminal driving, executed in the invoking
//! process. Arguments pass through whole; the drive runtime parses them, so
//! its help is written for the agents that read it.
use super::PlaybookPlugin;
use bmux_plugin_sdk::{NativeCommandContext, PluginCommandError};

pub(super) fn run(
    plugin: &PlaybookPlugin,
    ctx: &NativeCommandContext,
) -> Result<i32, PluginCommandError> {
    let host = plugin.interactive_host.clone().ok_or_else(|| {
        PluginCommandError::unavailable(
            "bmux drive requires the invoking application's execution adapters",
        )
    })?;
    host.run_drive(ctx.arguments.clone())
        .map(i32::from)
        .map_err(|e| PluginCommandError::failed(format!("{e:#}")))
}
