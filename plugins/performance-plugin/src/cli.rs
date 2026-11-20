//! Command-line policy; runtime settings remain authoritative at the selected service.
use bmux_performance_plugin_api::{performance_commands, performance_state};
use bmux_performance_state::{PerformanceRecordingLevel, PerformanceRuntimeSettings};
use bmux_plugin_sdk::{NativeCommandContext, PluginCommandError};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bmux perf")]
struct Cli {
    #[command(subcommand)]
    command: PerfCommand,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
enum PerfProfileArg {
    Basic,
    Detailed,
    Trace,
}

#[derive(Clone, Debug, Subcommand)]
enum PerfCommand {
    /// Show current runtime performance telemetry settings
    Status {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Enable runtime performance telemetry
    On {
        /// Capture profile level
        #[arg(long, value_enum, default_value_t = PerfProfileArg::Detailed)]
        profile: PerfProfileArg,
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Disable runtime performance telemetry
    Off {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
}

const fn performance_level_name(level: PerformanceRecordingLevel) -> &'static str {
    match level {
        PerformanceRecordingLevel::Off => "off",
        PerformanceRecordingLevel::Basic => "basic",
        PerformanceRecordingLevel::Detailed => "detailed",
        PerformanceRecordingLevel::Trace => "trace",
    }
}

const fn profile_to_level(profile: PerfProfileArg) -> PerformanceRecordingLevel {
    match profile {
        PerfProfileArg::Basic => PerformanceRecordingLevel::Basic,
        PerfProfileArg::Detailed => PerformanceRecordingLevel::Detailed,
        PerfProfileArg::Trace => PerformanceRecordingLevel::Trace,
    }
}

fn print_performance_settings(settings: &PerformanceRuntimeSettings) {
    println!(
        "runtime performance recording level: {}",
        performance_level_name(settings.recording_level)
    );
    println!("window ms: {}", settings.window_ms);
    println!("max events/sec: {}", settings.max_events_per_sec);
    println!(
        "max payload bytes/sec: {}",
        settings.max_payload_bytes_per_sec
    );
}

pub fn run(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let leaf = context
        .command
        .strip_prefix("perf-")
        .ok_or_else(|| PluginCommandError::unknown_command(&context.command))?;
    let cli = Cli::try_parse_from(
        ["bmux perf", leaf]
            .into_iter()
            .chain(context.arguments.iter().map(String::as_str)),
    )
    .map_err(|error| PluginCommandError::new(error.exit_code(), error.to_string()))?;
    let (level, json) = match cli.command {
        PerfCommand::Status { json } => (None, json),
        PerfCommand::On { profile, json } => (Some(profile_to_level(profile)), json),
        PerfCommand::Off { json } => (Some(PerformanceRecordingLevel::Off), json),
    };
    let mut client = bmux_plugin::ServiceCallerDispatchClient::new(context);
    let mut settings: PerformanceRuntimeSettings =
        bmux_plugin::block_on_typed_dispatch(performance_state::client::get_settings(&mut client))
            .map_err(|error| PluginCommandError::failed(error.to_string()))?
            .into();
    if let Some(level) = level {
        settings.recording_level = level;
        settings = bmux_plugin::block_on_typed_dispatch(
            performance_commands::client::set_settings(&mut client, settings.into()),
        )
        .map_err(|error| PluginCommandError::failed(error.to_string()))?
        .into();
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&settings)?);
    } else {
        match level {
            Some(PerformanceRecordingLevel::Off) => {
                println!("runtime performance telemetry disabled");
            }
            Some(_) => println!(
                "runtime performance telemetry enabled ({})",
                performance_level_name(settings.recording_level)
            ),
            None => (),
        }
        print_performance_settings(&settings);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_perf_status_json_flag() {
        let cli = Cli::try_parse_from(["bmux perf", "status", "--json"]).expect("valid CLI args");
        let command = cli.command;
        assert!(matches!(command, PerfCommand::Status { json: true }));
    }

    #[test]
    fn parses_perf_on_defaults_to_detailed_profile() {
        let cli = Cli::try_parse_from(["bmux perf", "on"]).expect("valid CLI args");
        let command = cli.command;
        assert!(matches!(
            command,
            PerfCommand::On {
                profile: PerfProfileArg::Detailed,
                json: false,
            }
        ));
    }

    #[test]
    fn parses_perf_on_with_trace_profile() {
        let cli = Cli::try_parse_from(["bmux perf", "on", "--profile", "trace", "--json"])
            .expect("valid CLI args");
        let command = cli.command;
        assert!(matches!(
            command,
            PerfCommand::On {
                profile: PerfProfileArg::Trace,
                json: true,
            }
        ));
    }

    #[test]
    fn parses_perf_off_json_flag() {
        let cli = Cli::try_parse_from(["bmux perf", "off", "--json"]).expect("valid CLI args");
        let command = cli.command;
        assert!(matches!(command, PerfCommand::Off { json: true }));
    }
}
