use crate::{diagnostics_watch, logs_tail, logs_watch};
use bmux_plugin_sdk::{NativeCommandContext, PluginCommandError};
use clap::{Parser, Subcommand};
use std::path::Path;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Logs {
        #[command(subcommand)]
        command: LogsCommand,
    },
    Diagnostics {
        #[command(subcommand)]
        command: DiagnosticsCommand,
    },
}
#[derive(Clone, Debug, clap::Args)]
pub struct WatchOptions {
    /// Number of recent entries to preload (defaults to saved profile value or 200)
    #[arg(long)]
    pub lines: Option<usize>,
    /// Show entries newer than a relative duration (e.g. 30s, 10m, 2h, 1d)
    #[arg(long)]
    pub since: Option<String>,
    /// State profile for saved watch filters (default: global `default`)
    #[arg(long)]
    pub profile: Option<String>,
    /// Include regex filter (case-sensitive, repeatable)
    #[arg(long = "include")]
    pub include: Vec<String>,
    /// Include regex filter (case-insensitive, repeatable)
    #[arg(long = "include-i")]
    pub include_i: Vec<String>,
    /// Exclude regex filter (case-sensitive, repeatable)
    #[arg(long = "exclude")]
    pub exclude: Vec<String>,
    /// Exclude regex filter (case-insensitive, repeatable)
    #[arg(long = "exclude-i")]
    pub exclude_i: Vec<String>,
}

#[derive(Clone, Debug, Subcommand)]
enum DiagnosticsCommand {
    /// Interactive structured diagnostics event viewer with dynamic filters
    Watch(WatchOptions),
    /// Alias for watch
    View(WatchOptions),
}

#[derive(Clone, Debug, Subcommand)]
pub enum LogsCommand {
    /// Print effective log file path
    Path {
        #[arg(long)]
        json: bool,
    },
    /// Print recent log lines and optionally follow updates
    Tail {
        /// Number of recent lines to show before follow
        #[arg(long, default_value_t = 50)]
        lines: usize,
        /// Show entries newer than a relative duration (e.g. 30s, 10m, 2h, 1d)
        #[arg(long)]
        since: Option<String>,
        /// Print recent lines only (disable follow)
        #[arg(long)]
        no_follow: bool,
    },
    /// Interactive live log viewer with dynamic filters
    Watch(WatchOptions),
    /// Manage saved log watch profiles
    Profiles {
        #[command(subcommand)]
        command: LogsProfilesCommand,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum LogsProfilesCommand {
    /// List saved watch profiles
    List {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show details for one profile
    Show {
        /// Profile name (default: global profile `default`)
        profile: Option<String>,
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Delete a saved profile
    Delete {
        /// Profile name to delete
        profile: String,
    },
    /// Rename a saved profile
    Rename {
        /// Existing profile name
        from: String,
        /// New profile name
        to: String,
    },
}

pub fn run(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let cli = Cli::try_parse_from(
        std::iter::once("bmux")
            .chain(context.command.split('-'))
            .chain(context.arguments.iter().map(String::as_str)),
    )
    .map_err(|error| PluginCommandError::new(error.exit_code(), error.to_string()))?;
    let state = Path::new(&context.connection.state_dir);
    if state.as_os_str().is_empty() {
        return Err(PluginCommandError::unavailable(
            "selected state directory missing",
        ));
    }
    let result = match cli.command {
        Command::Diagnostics {
            command: DiagnosticsCommand::Watch(options) | DiagnosticsCommand::View(options),
        } => diagnostics_watch::run_diagnostics_watch(state, options),
        Command::Logs { command } => match command {
            LogsCommand::Path { json } => logs_watch::run_logs_path(json),
            LogsCommand::Tail {
                lines,
                since,
                no_follow,
            } => logs_tail::run_logs_tail(lines, since.as_deref(), !no_follow),
            LogsCommand::Watch(options) => logs_watch::run_logs_watch(state, options),
            LogsCommand::Profiles { command } => match command {
                LogsProfilesCommand::List { json } => {
                    logs_watch::run_logs_profiles_list(state, json)
                }
                LogsProfilesCommand::Show { profile, json } => {
                    logs_watch::run_logs_profiles_show(state, profile.as_deref(), json)
                }
                LogsProfilesCommand::Delete { profile } => {
                    logs_watch::run_logs_profiles_delete(state, &profile)
                }
                LogsProfilesCommand::Rename { from, to } => {
                    logs_watch::run_logs_profiles_rename(state, &from, &to)
                }
            },
        },
    };
    result
        .map(i32::from)
        .map_err(|error| PluginCommandError::failed(format!("{error:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn viewer_manifest_arguments_match_parser_help() {
        use clap::CommandFactory;
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(include_str!("../plugin.toml")).unwrap();
        let root = Cli::command();
        for entry in manifest.commands.iter().filter(|c| {
            c.name == "diagnostics-watch" || c.name == "logs-watch" || c.name == "logs-tail"
        }) {
            let leaf = entry
                .path
                .iter()
                .fold(&root, |node, name| node.find_subcommand(name).unwrap());
            for arg in &entry.arguments {
                let parser = leaf
                    .get_arguments()
                    .find(|a| a.get_id().as_str() == arg.name.replace('-', "_"))
                    .unwrap();
                assert!(
                    arg.summary.as_ref().is_some_and(|s| !s.is_empty()),
                    "{}: {} lacks help",
                    entry.name,
                    arg.name
                );
                assert_eq!(
                    arg.default_value.as_deref(),
                    parser.get_default_values().first().and_then(|v| v.to_str())
                );
                if arg.long.is_some() && parser.get_action().takes_values() {
                    assert_eq!(
                        arg.value_name.as_deref(),
                        Some(arg.name.to_uppercase().replace('-', "_").as_str())
                    );
                }
            }
        }
    }

    #[test]
    fn parses_logs_tail_flags() {
        let cli = Cli::try_parse_from(["bmux", "logs", "tail", "--lines", "10", "--no-follow"])
            .expect("valid CLI args");
        let Command::Logs { command } = cli.command else {
            panic!("expected logs subcommand");
        };
        assert!(matches!(
            command,
            LogsCommand::Tail {
                lines: 10,
                since: None,
                no_follow: true
            }
        ));
    }

    #[test]
    fn parses_logs_since_filter() {
        let cli = Cli::try_parse_from(["bmux", "logs", "tail", "--since", "15m"])
            .expect("valid CLI args");
        let Command::Logs { command } = cli.command else {
            panic!("expected logs subcommand");
        };
        assert!(matches!(
            command,
            LogsCommand::Tail {
                lines: 50,
                since: Some(ref value),
                no_follow: false
            } if value == "15m"
        ));
    }

    #[test]
    fn parses_logs_watch_flags() {
        let cli = Cli::try_parse_from([
            "bmux",
            "logs",
            "watch",
            "--lines",
            "150",
            "--since",
            "2h",
            "--include",
            "server.*listening",
            "--include-i",
            "warn",
            "--exclude",
            "healthcheck",
            "--exclude-i",
            "noise",
        ])
        .expect("valid CLI args");
        let Command::Logs { command } = cli.command else {
            panic!("expected logs subcommand");
        };
        assert!(matches!(
            command,
            LogsCommand::Watch(WatchOptions {
                lines: Some(150),
                since: Some(ref value),
                profile: None,
                include,
                include_i,
                exclude,
                exclude_i,
            }) if value == "2h"
                && include == vec!["server.*listening"]
                && include_i == vec!["warn"]
                && exclude == vec!["healthcheck"]
                && exclude_i == vec!["noise"]
        ));
    }

    #[test]
    fn parses_logs_watch_profile_flag() {
        let cli = Cli::try_parse_from(["bmux", "logs", "watch", "--profile", "incident-db"])
            .expect("valid CLI args");
        let Command::Logs { command } = cli.command else {
            panic!("expected logs subcommand");
        };
        assert!(matches!(
            command,
            LogsCommand::Watch(WatchOptions {
                lines: None,
                since: None,
                profile: Some(ref value),
                include,
                include_i,
                exclude,
                exclude_i,
            }) if value == "incident-db"
                && include.is_empty()
                && include_i.is_empty()
                && exclude.is_empty()
                && exclude_i.is_empty()
        ));
    }

    #[test]
    fn parses_logs_profiles_list_json() {
        let cli = Cli::try_parse_from(["bmux", "logs", "profiles", "list", "--json"])
            .expect("valid CLI args");
        let Command::Logs { command } = cli.command else {
            panic!("expected logs subcommand");
        };
        assert!(matches!(
            command,
            LogsCommand::Profiles {
                command: LogsProfilesCommand::List { json: true }
            }
        ));
    }

    #[test]
    fn parses_logs_profiles_show_default() {
        let cli =
            Cli::try_parse_from(["bmux", "logs", "profiles", "show"]).expect("valid CLI args");
        let Command::Logs { command } = cli.command else {
            panic!("expected logs subcommand");
        };
        assert!(matches!(
            command,
            LogsCommand::Profiles {
                command: LogsProfilesCommand::Show {
                    profile: None,
                    json: false
                }
            }
        ));
    }

    #[test]
    fn parses_logs_profiles_delete_and_rename() {
        let delete_cli = Cli::try_parse_from(["bmux", "logs", "profiles", "delete", "incident-db"])
            .expect("valid CLI args");
        assert!(matches!(
            delete_cli.command,
            Command::Logs {
                command:
                    LogsCommand::Profiles {
                        command: LogsProfilesCommand::Delete { profile }
                    }
            } if profile == "incident-db"
        ));

        let rename_cli = Cli::try_parse_from([
            "bmux",
            "logs",
            "profiles",
            "rename",
            "incident-db",
            "incident-db-2",
        ])
        .expect("valid CLI args");
        assert!(matches!(
            rename_cli.command,
            Command::Logs {
                command:
                    LogsCommand::Profiles {
                        command: LogsProfilesCommand::Rename { from, to }
                    }
            } if from == "incident-db" && to == "incident-db-2"
        ));
    }
}
