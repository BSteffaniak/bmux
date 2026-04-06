use anyhow::{Context, Result};
use bmux_cli_schema::{Cli, LogLevel};
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_plugin::PluginRegistry;
use clap::{CommandFactory, FromArgMatches};
use tracing::Level;

use super::{
    effective_enabled_plugins, plugin_commands, plugin_commands::PluginCommandRegistry,
    scan_available_plugins,
};

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(super) enum ParsedRuntimeCli {
    BuiltIn {
        cli: Cli,
        log_level: LogLevel,
        verbose: bool,
    },
    ImmediateExit {
        code: u8,
        output: String,
        stderr: bool,
    },
    Plugin {
        log_level: LogLevel,
        plugin_id: String,
        command_name: String,
        arguments: Vec<String>,
    },
}

pub(super) fn parse_runtime_cli() -> Result<ParsedRuntimeCli> {
    let argv = std::env::args_os().collect::<Vec<_>>();
    apply_runtime_override_from_raw_args(&argv)?;
    let config = BmuxConfig::load()?;
    let paths = ConfigPaths::default();
    let registry = scan_available_plugins(&config, &paths)?;
    parse_runtime_cli_with_registry(&argv, &config, &registry)
}

fn apply_runtime_override_from_raw_args(argv: &[std::ffi::OsString]) -> Result<()> {
    let mut index = 1usize;
    while index < argv.len() {
        let arg = argv[index].to_string_lossy();
        if let Some(value) = arg.strip_prefix("--runtime=") {
            let runtime = validate_runtime_name(value)?;
            // SAFETY: this runs during CLI bootstrap before background tasks/threads are spawned.
            unsafe { std::env::set_var("BMUX_RUNTIME_NAME", runtime) };
            return Ok(());
        }
        if arg == "--runtime" {
            let Some(value) = argv.get(index + 1) else {
                anyhow::bail!("--runtime requires a value")
            };
            let runtime = validate_runtime_name(&value.to_string_lossy())?;
            // SAFETY: this runs during CLI bootstrap before background tasks/threads are spawned.
            unsafe { std::env::set_var("BMUX_RUNTIME_NAME", runtime) };
            return Ok(());
        }
        index += 1;
    }
    Ok(())
}

fn validate_runtime_name(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        anyhow::bail!("runtime name cannot be empty")
    }
    if trimmed == "default" {
        return Ok(trimmed.to_string());
    }
    if trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Ok(trimmed.to_string());
    }
    anyhow::bail!("runtime name can only include letters, numbers, '-', '_' or '.'")
}

pub(super) fn parse_runtime_cli_with_registry(
    argv: &[std::ffi::OsString],
    config: &BmuxConfig,
    registry: &PluginRegistry,
) -> Result<ParsedRuntimeCli> {
    let mut command_config = config.clone();
    command_config.plugins.enabled = effective_enabled_plugins(config, registry);
    let command_registry = PluginCommandRegistry::build(&command_config, registry)
        .context("failed building plugin CLI command registry")?;
    if let Some(raw_args) = argv
        .iter()
        .skip(1)
        .map(|arg| arg.to_str().map(ToString::to_string))
        .collect::<Option<Vec<_>>>()
        && let Some(resolved) = command_registry.resolve(&raw_args)
    {
        let normalized =
            PluginCommandRegistry::validate_arguments(&resolved.schema, &resolved.arguments)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let verbose = raw_args.iter().any(|arg| arg == "--verbose" || arg == "-v");
        let log_level = resolve_log_level(
            verbose,
            None,
            std::env::var("BMUX_LOG_LEVEL").ok().as_deref(),
        );
        if !raw_args.iter().any(|arg| arg == "--core-builtins-only") {
            return Ok(ParsedRuntimeCli::Plugin {
                log_level,
                plugin_id: resolved.plugin_id,
                command_name: resolved.command_name,
                arguments: normalized,
            });
        }
    }
    let clap_command = command_registry
        .augment_clap_command(Cli::command())
        .context("failed augmenting CLI with plugin commands")?;
    let matches = match clap_command.try_get_matches_from(argv.iter().cloned()) {
        Ok(matches) => matches,
        Err(error) => {
            return Ok(match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                    ParsedRuntimeCli::ImmediateExit {
                        code: 0,
                        output: error.to_string(),
                        stderr: false,
                    }
                }
                _ => ParsedRuntimeCli::ImmediateExit {
                    code: 2,
                    output: error.to_string(),
                    stderr: true,
                },
            });
        }
    };
    let verbose = matches.get_flag("verbose");
    let log_level = resolve_log_level(
        verbose,
        matches.get_one::<LogLevel>("log_level").copied(),
        std::env::var("BMUX_LOG_LEVEL").ok().as_deref(),
    );
    let core_builtins_only = matches.get_flag("core_builtins_only");
    if !core_builtins_only {
        let (path, leaf_matches) = plugin_commands::selected_subcommand_path(&matches);
        if let Some(resolved) = command_registry.resolve_exact_path(&path) {
            let arguments = PluginCommandRegistry::normalize_arguments_from_matches(
                &resolved.schema,
                leaf_matches,
            );
            return Ok(ParsedRuntimeCli::Plugin {
                log_level,
                plugin_id: resolved.plugin_id,
                command_name: resolved.command_name,
                arguments,
            });
        }
    }

    let cli =
        Cli::from_arg_matches(&matches).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(ParsedRuntimeCli::BuiltIn {
        cli,
        log_level,
        verbose,
    })
}

pub(super) fn resolve_log_level(
    verbose: bool,
    cli_level: Option<LogLevel>,
    env_level: Option<&str>,
) -> LogLevel {
    if let Some(level) = cli_level {
        return level;
    }
    if verbose {
        return LogLevel::Debug;
    }
    if let Some(raw) = env_level
        && let Some(level) = parse_log_level(raw)
    {
        return level;
    }
    LogLevel::Info
}

pub(super) fn parse_log_level(raw: &str) -> Option<LogLevel> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "error" => Some(LogLevel::Error),
        "warn" | "warning" => Some(LogLevel::Warn),
        "info" => Some(LogLevel::Info),
        "debug" => Some(LogLevel::Debug),
        "trace" => Some(LogLevel::Trace),
        _ => None,
    }
}

pub(super) const fn tracing_level(level: LogLevel) -> Level {
    match level {
        LogLevel::Error => Level::ERROR,
        LogLevel::Warn => Level::WARN,
        LogLevel::Info => Level::INFO,
        LogLevel::Debug => Level::DEBUG,
        LogLevel::Trace => Level::TRACE,
    }
}

pub(super) fn validate_record_bootstrap_flags(cli: &Cli) -> Result<()> {
    if cli.command.is_some() {
        if cli.record {
            anyhow::bail!(
                "--record is only supported for top-level interactive start (no subcommand)"
            )
        }
        if cli.no_capture_input {
            anyhow::bail!("--no-capture-input requires --record")
        }
        if cli.recording_id_file.is_some() {
            anyhow::bail!("--recording-id-file requires --record")
        }
        if cli.record_profile.is_some() {
            anyhow::bail!("--record-profile requires --record")
        }
        if cli.record_name.is_some() {
            anyhow::bail!("--record-name requires --record")
        }
        if !cli.record_event_kind.is_empty() {
            anyhow::bail!("--record-event-kind requires --record")
        }
        if cli.stop_server_on_exit {
            anyhow::bail!("--stop-server-on-exit requires --record")
        }
    } else if !cli.record {
        if cli.no_capture_input {
            anyhow::bail!("--no-capture-input requires --record")
        }
        if cli.recording_id_file.is_some() {
            anyhow::bail!("--recording-id-file requires --record")
        }
        if cli.record_profile.is_some() {
            anyhow::bail!("--record-profile requires --record")
        }
        if cli.record_name.is_some() {
            anyhow::bail!("--record-name requires --record")
        }
        if !cli.record_event_kind.is_empty() {
            anyhow::bail!("--record-event-kind requires --record")
        }
        if cli.stop_server_on_exit {
            anyhow::bail!("--stop-server-on-exit requires --record")
        }
    }
    Ok(())
}
