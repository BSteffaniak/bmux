//! Socket-controlled sandbox command, executed in the invoking process.
use super::PlaybookPlugin;
use anyhow::Result;
use bmux_plugin_sdk::{NativeCommandContext, PluginCommandError};
use clap::Parser;

#[derive(Parser)]
#[command(name = "bmux playbook interactive")]
struct Args {
    /// Socket path override (default: auto-generated in sandbox temp dir)
    #[arg(long)]
    socket: Option<String>,
    /// Record the session
    #[arg(long)]
    record: bool,
    /// Viewport dimensions as COLSxROWS (default: 80x24)
    #[arg(long, default_value = "80x24")]
    viewport: String,
    /// Shell override
    #[arg(long)]
    shell: Option<String>,
    /// Max session lifetime in seconds (default: no limit)
    #[arg(long)]
    timeout: Option<u64>,
}

pub(super) fn run(
    plugin: &PlaybookPlugin,
    ctx: &NativeCommandContext,
) -> Result<i32, PluginCommandError> {
    if !ctx.invocation_source.is_cli() {
        return Err(PluginCommandError::failed(
            "interactive playbook requires a standalone CLI invocation",
        ));
    }
    let args = Args::try_parse_from(
        std::iter::once("bmux playbook interactive")
            .chain(ctx.arguments.iter().map(String::as_str)),
    )
    .map_err(|e| PluginCommandError::new(e.exit_code(), e.to_string()))?;
    let host = plugin.host.clone().ok_or_else(|| {
        PluginCommandError::unavailable(
            "interactive playbook requires the invoking application's execution adapters",
        )
    })?;
    let viewport = bmux_playbook::parse_viewport_string(&args.viewport)
        .map_err(|e| PluginCommandError::failed(format!("{e:#}")))?;
    host.run_controller(
        args.socket.as_deref(),
        args.record,
        viewport,
        args.shell.as_deref(),
        args.timeout.map(std::time::Duration::from_secs),
    )
    .map(i32::from)
    .map_err(|e| PluginCommandError::failed(format!("{e:#}")))
}
