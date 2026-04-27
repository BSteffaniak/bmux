use anyhow::{Context, Result};
use bmux_cli_schema::{Cli, LogLevel};
use bmux_config::{BmuxConfig, ConfigLoadOverrides, ConfigPaths, RECORDINGS_DIR_OVERRIDE_ENV};
use bmux_plugin::PluginRegistry;
use bmux_plugin_sdk::perf_telemetry::{PhaseChannel, PhasePayload, emit as emit_phase_timing};
use clap::{CommandFactory, FromArgMatches};
use std::sync::Arc;
use std::time::Instant;
use tracing::Level;

pub(super) const RECORDING_AUTO_EXPORT_OVERRIDE_ENV: &str = "BMUX_RECORDING_AUTO_EXPORT";
pub(super) const RECORDING_AUTO_EXPORT_DIR_OVERRIDE_ENV: &str = "BMUX_RECORDING_AUTO_EXPORT_DIR";

use super::plugin_runtime::{RuntimeCommandState, build_runtime_command_state};
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
        config_overrides: ConfigLoadOverrides,
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
        command_state: Option<RuntimeCommandState>,
        config_overrides: ConfigLoadOverrides,
    },
}

#[derive(Debug, Clone, Default)]
struct RawRuntimeOverrides {
    config_path: Option<std::path::PathBuf>,
}

pub(super) fn parse_runtime_cli() -> Result<ParsedRuntimeCli> {
    let total_started = Instant::now();
    let argv_started = Instant::now();
    let argv = std::env::args_os().collect::<Vec<_>>();
    let raw_overrides = apply_runtime_override_from_raw_args(&argv)?;
    let argv_us = argv_started.elapsed().as_micros();
    let slot_started = Instant::now();
    // Resolve the active slot (if any) before loading config, so that
    // slot-aware paths / env propagation apply to the rest of bootstrap.
    let slot_state = super::slot::active_slot().clone();
    if let super::slot::ActiveSlotState::Resolved { slot, .. } = &slot_state {
        // Propagate BMUX_SLOT_NAME so child processes (daemon, sandboxed
        // re-execs, etc.) inherit the active slot name.
        // SAFETY: CLI bootstrap, before threads spawn.
        unsafe { std::env::set_var(bmux_slots::SLOT_NAME_ENV, &slot.name) };
    }
    if let super::slot::ActiveSlotState::Unknown { name, known, .. } = &slot_state {
        anyhow::bail!(
            "active slot {name:?} is not declared in the slot manifest (known: {known:?}). \
             Add it to slots.toml or unset BMUX_SLOT_NAME."
        );
    }
    let slot_us = slot_started.elapsed().as_micros();
    let config_started = Instant::now();
    let mut config_overrides = ConfigLoadOverrides::from_env_with_cli(raw_overrides.config_path);
    // When a slot is active and `inherit_base = true`, layer the shared
    // `<config_root>/base.toml` underneath the slot's config.
    if let super::slot::ActiveSlotState::Resolved { slot, .. } = &slot_state
        && slot.inherit_base
    {
        config_overrides.base_config_path = Some(bmux_slots::default_base_config_path());
    }
    let (config, paths) = if let super::slot::ActiveSlotState::Resolved { slot, .. } = &slot_state {
        let paths = ConfigPaths::for_slot(slot);
        let cfg = BmuxConfig::load_with_paths_and_overrides(&paths, &config_overrides)?;
        (cfg, paths)
    } else {
        let cfg = BmuxConfig::load_with_overrides(&config_overrides)?;
        (cfg, ConfigPaths::default())
    };
    let config_us = config_started.elapsed().as_micros();
    let scan_started = Instant::now();
    let registry = Arc::new(scan_available_plugins(&config, &paths)?);
    let scan_us = scan_started.elapsed().as_micros();
    let state_started = Instant::now();
    let command_state = build_runtime_command_state(config.clone(), paths, Arc::clone(&registry))?;
    let state_us = state_started.elapsed().as_micros();
    let parse_started = Instant::now();
    let parsed = parse_runtime_cli_with_registry(
        &argv,
        &config,
        &registry,
        Some(command_state),
        config_overrides,
    )?;
    emit_parse_phase_timing(
        argv_us,
        slot_us,
        config_us,
        scan_us,
        state_us,
        parse_started.elapsed().as_micros(),
        total_started.elapsed().as_micros(),
    );
    Ok(parsed)
}

#[allow(clippy::too_many_arguments)]
fn emit_parse_phase_timing(
    argv_us: u128,
    slot_us: u128,
    config_us: u128,
    scan_us: u128,
    state_us: u128,
    parse_us: u128,
    total_us: u128,
) {
    let payload = PhasePayload::new("parse_runtime_cli")
        .field("argv_us", argv_us)
        .field("slot_us", slot_us)
        .field("config_us", config_us)
        .field("scan_us", scan_us)
        .field("state_us", state_us)
        .field("parse_us", parse_us)
        .field("total_us", total_us)
        .finish();
    emit_phase_timing(PhaseChannel::Plugin, &payload);
}

fn apply_runtime_override_from_raw_args(
    argv: &[std::ffi::OsString],
) -> Result<RawRuntimeOverrides> {
    let mut overrides = RawRuntimeOverrides::default();
    let mut index = 1usize;
    while index < argv.len() {
        let arg = argv[index].to_string_lossy();
        if arg == "--" {
            break;
        }
        if let Some(value) = arg.strip_prefix("--runtime=") {
            let runtime = validate_runtime_name(value)?;
            // SAFETY: this runs during CLI bootstrap before background tasks/threads are spawned.
            unsafe { std::env::set_var("BMUX_RUNTIME_NAME", runtime) };
            index += 1;
            continue;
        }
        if arg == "--runtime" {
            let Some(value) = argv.get(index + 1) else {
                anyhow::bail!("--runtime requires a value")
            };
            let runtime = validate_runtime_name(&value.to_string_lossy())?;
            // SAFETY: this runs during CLI bootstrap before background tasks/threads are spawned.
            unsafe { std::env::set_var("BMUX_RUNTIME_NAME", runtime) };
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--config=") {
            let path = resolve_cli_path_override(value, "--config")?;
            overrides.config_path = Some(std::path::PathBuf::from(path));
            index += 1;
            continue;
        }
        if arg == "--config" {
            let Some(value) = argv.get(index + 1) else {
                anyhow::bail!("--config requires a value")
            };
            let path = resolve_cli_path_override(&value.to_string_lossy(), "--config")?;
            overrides.config_path = Some(std::path::PathBuf::from(path));
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--recordings-dir=") {
            let path = resolve_cli_path_override(value, "--recordings-dir")?;
            // SAFETY: this runs during CLI bootstrap before background tasks/threads are spawned.
            unsafe { std::env::set_var(RECORDINGS_DIR_OVERRIDE_ENV, path) };
            index += 1;
            continue;
        }
        if arg == "--recordings-dir" {
            let Some(value) = argv.get(index + 1) else {
                anyhow::bail!("--recordings-dir requires a value")
            };
            let path = resolve_cli_path_override(&value.to_string_lossy(), "--recordings-dir")?;
            // SAFETY: this runs during CLI bootstrap before background tasks/threads are spawned.
            unsafe { std::env::set_var(RECORDINGS_DIR_OVERRIDE_ENV, path) };
            index += 2;
            continue;
        }
        if arg == "--recording-auto-export" {
            // SAFETY: this runs during CLI bootstrap before background tasks/threads are spawned.
            unsafe { std::env::set_var(RECORDING_AUTO_EXPORT_OVERRIDE_ENV, "1") };
            index += 1;
            continue;
        }
        if arg == "--no-recording-auto-export" {
            // SAFETY: this runs during CLI bootstrap before background tasks/threads are spawned.
            unsafe { std::env::set_var(RECORDING_AUTO_EXPORT_OVERRIDE_ENV, "0") };
            index += 1;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--recording-auto-export-dir=") {
            let path = resolve_cli_path_override(value, "--recording-auto-export-dir")?;
            // SAFETY: this runs during CLI bootstrap before background tasks/threads are spawned.
            unsafe { std::env::set_var(RECORDING_AUTO_EXPORT_DIR_OVERRIDE_ENV, path) };
            index += 1;
            continue;
        }
        if arg == "--recording-auto-export-dir" {
            let Some(value) = argv.get(index + 1) else {
                anyhow::bail!("--recording-auto-export-dir requires a value")
            };
            let path =
                resolve_cli_path_override(&value.to_string_lossy(), "--recording-auto-export-dir")?;
            // SAFETY: this runs during CLI bootstrap before background tasks/threads are spawned.
            unsafe { std::env::set_var(RECORDING_AUTO_EXPORT_DIR_OVERRIDE_ENV, path) };
            index += 2;
            continue;
        }
        index += 1;
    }
    Ok(overrides)
}

fn resolve_cli_path_override(value: &str, flag: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        anyhow::bail!("{flag} requires a non-empty path")
    }
    let path = std::path::PathBuf::from(trimmed);
    let resolved = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .with_context(|| format!("failed resolving relative path for {flag}"))?
            .join(path)
    };
    Ok(resolved.to_string_lossy().into_owned())
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

#[allow(clippy::too_many_lines)] // Single parse flow keeps fast-path/fallback ownership clear.
pub(super) fn parse_runtime_cli_with_registry(
    argv: &[std::ffi::OsString],
    config: &BmuxConfig,
    registry: &PluginRegistry,
    command_state: Option<RuntimeCommandState>,
    config_overrides: ConfigLoadOverrides,
) -> Result<ParsedRuntimeCli> {
    let total_started = Instant::now();
    let mut command_config = config.clone();
    command_config.plugins.enabled = command_state.as_ref().map_or_else(
        || effective_enabled_plugins(config, registry),
        |state| state.enabled_plugins.clone(),
    );
    let registry_started = Instant::now();
    let command_registry = PluginCommandRegistry::build(&command_config, registry)
        .context("failed building plugin CLI command registry")?;
    let registry_us = registry_started.elapsed().as_micros();
    let resolve_started = Instant::now();
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
            emit_parse_with_registry_phase_timing(
                registry_us,
                resolve_started.elapsed().as_micros(),
                0,
                0,
                total_started.elapsed().as_micros(),
            );
            return Ok(ParsedRuntimeCli::Plugin {
                log_level,
                plugin_id: resolved.plugin_id,
                command_name: resolved.command_name,
                arguments: normalized,
                command_state,
                config_overrides,
            });
        }
    }
    let resolve_us = resolve_started.elapsed().as_micros();
    let clap_augment_started = Instant::now();
    let clap_command = command_registry
        .augment_clap_command(Cli::command())
        .context("failed augmenting CLI with plugin commands")?;
    let clap_augment_us = clap_augment_started.elapsed().as_micros();
    let clap_parse_started = Instant::now();
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
    let clap_parse_us = clap_parse_started.elapsed().as_micros();
    emit_parse_with_registry_phase_timing(
        registry_us,
        resolve_us,
        clap_augment_us,
        clap_parse_us,
        total_started.elapsed().as_micros(),
    );
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
                command_state,
                config_overrides,
            });
        }
    }

    let cli =
        Cli::from_arg_matches(&matches).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(ParsedRuntimeCli::BuiltIn {
        cli,
        log_level,
        verbose,
        config_overrides,
    })
}

fn emit_parse_with_registry_phase_timing(
    registry_us: u128,
    resolve_us: u128,
    clap_augment_us: u128,
    clap_parse_us: u128,
    total_us: u128,
) {
    let payload = PhasePayload::new("parse_runtime_cli_with_registry")
        .field("command_registry_us", registry_us)
        .field("resolve_us", resolve_us)
        .field("clap_augment_us", clap_augment_us)
        .field("clap_parse_us", clap_parse_us)
        .field("total_us", total_us)
        .finish();
    emit_phase_timing(PhaseChannel::Plugin, &payload);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_arg_config_override_returns_resolved_path() {
        let args = vec![
            std::ffi::OsString::from("bmux"),
            std::ffi::OsString::from("--config"),
            std::ffi::OsString::from("./test.toml"),
        ];
        let overrides = apply_runtime_override_from_raw_args(&args).expect("apply overrides");
        let path = overrides.config_path.expect("config path override set");
        assert!(path.ends_with("test.toml"));
    }
}
