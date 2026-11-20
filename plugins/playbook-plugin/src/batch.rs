use super::PlaybookPlugin;
use anyhow::{Context, Result};
use bmux_plugin::HostRuntimeApi;
use bmux_plugin_sdk::{NativeCommandContext, PluginCliCommandRequest, PluginCommandError};
use clap::Parser;
use std::time::Duration;
#[derive(Parser)]
#[command(name = "bmux playbook run")]
struct Args {
    /// Path to playbook file, or `-` for stdin
    source: String,
    /// Output results as JSON
    #[arg(long)]
    json: bool,
    /// Run visual interactive live tour (TTY; non-TTY falls back to prompt controls)
    #[arg(long)]
    interactive: bool,
    /// Deterministic presentation pacing; automation defaults to instant
    #[arg(long, default_value = "instant", value_parser = ["instant", "half", "normal", "double"])]
    playback_speed: String,
    /// Run against the live server instead of an ephemeral sandbox
    #[arg(long)]
    target_server: bool,
    /// Record the playbook execution (overrides playbook config)
    #[arg(long)]
    record: bool,
    /// Export the recording as a GIF to the given path (implies --record)
    #[arg(long)]
    export_gif: Option<String>,
    /// Override viewport dimensions as COLSxROWS (e.g. 120x40)
    #[arg(long)]
    viewport: Option<String>,
    /// Override max playbook timeout in seconds
    #[arg(long)]
    timeout: Option<u64>,
    /// Override shell
    #[arg(long)]
    shell: Option<String>,
    /// Define a variable (repeatable). Format: KEY=VALUE
    #[arg(long = "var")]
    vars: Vec<String>,
    /// Print step-by-step progress to stderr
    #[arg(long, short)]
    verbose: bool,
}
pub(super) fn run(
    plugin: &PlaybookPlugin,
    ctx: &NativeCommandContext,
) -> Result<i32, PluginCommandError> {
    let args = Args::try_parse_from(
        std::iter::once("bmux playbook run").chain(ctx.arguments.iter().map(String::as_str)),
    )
    .map_err(|e| PluginCommandError::new(e.exit_code(), e.to_string()))?;
    let host = plugin.batch_host.clone().ok_or_else(|| {
        PluginCommandError::unavailable(
            "playbook run requires the invoking application's execution adapters",
        )
    })?;
    execute(ctx, host, args)
        .map(i32::from)
        .map_err(|e| PluginCommandError::failed(format!("{e:#}")))
}
fn execute(ctx: &NativeCommandContext, host: super::ExecutionHost, args: Args) -> Result<u8> {
    let Args {
        source,
        json,
        interactive,
        playback_speed,
        target_server,
        record,
        export_gif,
        viewport,
        timeout,
        shell,
        vars,
        verbose,
    } = args;
    let source = source.as_str();
    let playback_speed = playback_speed.as_str();
    let export_gif = export_gif.as_deref();
    let viewport = viewport.as_deref();
    let shell = shell.as_deref();
    let cli_vars = &vars;
    if let Some(probe) = std::env::var_os("BMUX_PLAYBOOK_PROTOCOL_PROBE") {
        let probe = std::path::PathBuf::from(probe);
        anyhow::ensure!(probe.is_absolute(), "worker probe path must be absolute");
        std::fs::write(probe, b"bmux-playbook-worker-v1")?;
        return Ok(0);
    }
    let parsed = if source == "-" {
        bmux_playbook::parser::parse_stdin().context("failed parsing playbook from stdin")
    } else {
        bmux_playbook::parser::parse_file(std::path::Path::new(source))
            .with_context(|| format!("failed parsing playbook from {source}"))
    };
    let mut playbook = match parsed {
        Ok(playbook) => playbook,
        Err(error) => {
            if let Some(control) = bmux_playbook::managed::ManagedRun::from_env()? {
                control.publish_failure(&format!("{error:#}"))?;
            }
            return Err(error);
        }
    };

    // CLI flags override playbook config.
    if record || export_gif.is_some() {
        playbook.config.record = true;
    }
    if let Some(vp) = viewport {
        let (cols, rows) = bmux_playbook::parse_viewport_string(vp)?;
        playbook.config.viewport.cols = cols;
        playbook.config.viewport.rows = rows;
    }
    if let Some(secs) = timeout {
        playbook.config.timeout = Duration::from_secs(secs);
    }
    if let Some(sh) = shell {
        playbook.config.shell = Some(sh.to_string());
    }

    // CLI --var overrides @var directives.
    for var_str in cli_vars {
        if let Some(eq_pos) = var_str.find('=') {
            let key = var_str[..eq_pos].to_string();
            let value = var_str[eq_pos + 1..].to_string();
            playbook.config.vars.insert(key, value);
        } else {
            anyhow::bail!("invalid --var format: expected KEY=VALUE, got '{var_str}'");
        }
    }

    // Populate bundled plugin IDs so the sandbox can configure plugins.
    playbook.config.bundled_plugin_ids = ctx
        .registered_plugins
        .iter()
        .filter(|info| info.bundled_static)
        .map(|info| info.id.clone())
        .collect();
    playbook.config.verbose = verbose;

    let speed =
        bmux_playbook::PlaybackSpeed::parse(playback_speed).context("invalid playback speed")?;
    // Real attachments run caller-process plugin commands with the invoking
    // command runtime, handed to the host explicitly (never rediscovered).
    let result = host.run_batch(
        playbook,
        target_server,
        bmux_playbook::RunOptions {
            interactive,
            playback_speed: speed,
        },
    )?;

    // Export GIF if requested and a recording was produced.
    if let Some(gif_path) = export_gif {
        if let Some(ref rec_id) = result.recording_id {
            let recording_id_str = rec_id.to_string();
            let args = vec![
                recording_id_str,
                "--format".to_string(),
                "gif".to_string(),
                "--output".to_string(),
                gif_path.to_string(),
            ];
            let export = ctx
                .plugin_command_run(&PluginCliCommandRequest::new(
                    "bmux.recording".into(),
                    "recording-export".into(),
                    args,
                ))
                .map_err(|e| anyhow::anyhow!("{e}"))
                .and_then(|response| match response.error {
                    Some(error) => Err(anyhow::anyhow!(error)),
                    None => Ok(response.exit_code),
                });
            match export {
                Ok(_) => {
                    if !json {
                        println!("exported GIF: {gif_path}");
                    }
                }
                Err(e) => {
                    eprintln!("GIF export failed: {e:#}");
                }
            }
        } else if !json {
            eprintln!("GIF export skipped: no recording was produced");
        }
    }

    if json {
        let json_str =
            serde_json::to_string_pretty(&result).context("failed serializing playbook result")?;
        println!("{json_str}");
    } else {
        print!("{}", bmux_playbook::format_result(&result));
    }

    Ok(u8::from(!result.pass))
}

#[derive(Parser)]
#[command(name = "bmux playbook cleanup")]
struct Cleanup {
    /// Only list orphaned dirs without deleting
    #[arg(long)]
    dry_run: bool,
    /// Output as JSON
    #[arg(long)]
    json: bool,
}
pub(super) fn cleanup(
    plugin: &PlaybookPlugin,
    ctx: &NativeCommandContext,
) -> Result<i32, PluginCommandError> {
    let args = Cleanup::try_parse_from(
        std::iter::once("bmux playbook cleanup").chain(ctx.arguments.iter().map(String::as_str)),
    )
    .map_err(|e| PluginCommandError::new(e.exit_code(), e.to_string()))?;
    let cleanup = plugin
        .cleanup
        .ok_or_else(|| PluginCommandError::unavailable("sandbox maintenance is not available"))?;
    cleanup(args.dry_run, false, None, Some("playbook"), args.json)
        .map(i32::from)
        .map_err(|e| PluginCommandError::failed(format!("{e:#}")))
}
#[cfg(test)]
mod tests {
    use super::{Args, Parser};
    #[test]
    fn playback_args() {
        let args = Args::try_parse_from(["run", "demo.dsl", "--interactive"]).unwrap();
        assert!(args.interactive);
        assert_eq!(args.playback_speed, "instant");
        for speed in ["instant", "half", "normal", "double"] {
            assert_eq!(
                Args::try_parse_from(["run", "demo.dsl", "--playback-speed", speed])
                    .unwrap()
                    .playback_speed,
                speed
            );
        }
        assert!(Args::try_parse_from(["run", "demo.dsl", "--playback-speed", "fast"]).is_err());
    }
}
