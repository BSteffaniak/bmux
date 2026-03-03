use clap::{Parser, Subcommand, ValueEnum};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum DebugRenderLogFormat {
    Text,
    Csv,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum TraceFamily {
    Csi,
    Osc,
    Dcs,
}

#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
#[command(name = "bmux")]
#[command(about = "A minimal fullscreen PTY runtime for bmux")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,

    /// Enable verbose logging
    #[arg(short, long)]
    pub(crate) verbose: bool,

    /// Shell binary to launch inside the PTY
    #[arg(long)]
    pub(crate) shell: Option<String>,

    /// Disable alternate screen mode (debug fallback)
    #[arg(long)]
    pub(crate) no_alt_screen: bool,

    /// Show live render diagnostics in status bar
    #[arg(long)]
    pub(crate) debug_render: bool,

    /// Append render diagnostics to a log file
    #[arg(long, value_name = "PATH")]
    pub(crate) debug_render_log: Option<std::path::PathBuf>,

    /// Render diagnostics log format
    #[arg(long, value_enum, default_value_t = DebugRenderLogFormat::Text)]
    pub(crate) debug_render_log_format: DebugRenderLogFormat,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Create a new session
    NewSession {
        /// Optional session name
        name: Option<String>,
    },
    /// List active sessions
    ListSessions {
        /// Print sessions as JSON
        #[arg(long)]
        json: bool,
    },
    /// Kill a session by name or UUID
    KillSession {
        /// Session name or UUID
        target: String,
    },
    /// Attach to a session by name or UUID
    Attach {
        /// Session name or UUID
        target: String,
    },
    /// Detach from the current session
    Detach,
    /// Session management commands
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Server lifecycle and status tools
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    /// Keymap tools and diagnostics
    Keymap {
        #[command(subcommand)]
        command: KeymapCommand,
    },
    /// Layout state tools
    Layout {
        #[command(subcommand)]
        command: LayoutCommand,
    },
    /// Terminal capability tools and diagnostics
    Terminal {
        #[command(subcommand)]
        command: TerminalCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum SessionCommand {
    /// Create a new session
    New {
        /// Optional session name
        name: Option<String>,
    },
    /// List active sessions
    List {
        /// Print sessions as JSON
        #[arg(long)]
        json: bool,
    },
    /// Kill a session by name or UUID
    Kill {
        /// Session name or UUID
        target: String,
    },
    /// Attach to a session by name or UUID
    Attach {
        /// Session name or UUID
        target: String,
    },
    /// Detach from the current session
    Detach,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ServerCommand {
    /// Start local bmux server
    Start {
        /// Run server in background daemon mode
        #[arg(long)]
        daemon: bool,
        /// Internal flag used by daemon launcher
        #[arg(long, hide = true)]
        foreground_internal: bool,
    },
    /// Check server status
    Status,
    /// Request graceful server shutdown
    Stop,
}

#[derive(Debug, Subcommand)]
pub(crate) enum KeymapCommand {
    /// Print compiled keymap and overlap diagnostics
    Doctor {
        /// Print diagnostics as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum LayoutCommand {
    /// Clear persisted local runtime layout state
    Clear,
}

#[derive(Debug, Subcommand)]
pub(crate) enum TerminalCommand {
    /// Show terminal capability profile used for panes
    Doctor {
        /// Print diagnostics as JSON
        #[arg(long)]
        json: bool,
        /// Include recent protocol trace events
        #[arg(long)]
        trace: bool,
        /// Limit number of trace events shown
        #[arg(long, default_value_t = 50)]
        trace_limit: usize,
        /// Filter trace events by protocol family
        #[arg(long, value_enum)]
        trace_family: Option<TraceFamily>,
        /// Filter trace events by pane id
        #[arg(long)]
        trace_pane: Option<u16>,
    },
    /// Install bmux-256color terminfo entry
    InstallTerminfo {
        /// Proceed without interactive confirmation
        #[arg(long)]
        yes: bool,
        /// Check installability/status without installing
        #[arg(long)]
        check: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        Cli, Command, KeymapCommand, LayoutCommand, ServerCommand, SessionCommand, TerminalCommand,
        TraceFamily,
    };
    use clap::Parser;

    #[test]
    fn parses_no_alt_screen_flag() {
        let cli = Cli::try_parse_from(["bmux", "--no-alt-screen"]).expect("valid CLI args");
        assert!(cli.no_alt_screen);
    }

    #[test]
    fn parses_shell_flag() {
        let cli = Cli::try_parse_from(["bmux", "--shell", "/bin/sh"]).expect("valid CLI args");
        assert_eq!(cli.shell.as_deref(), Some("/bin/sh"));
    }

    #[test]
    fn parses_debug_render_flag() {
        let cli = Cli::try_parse_from(["bmux", "--debug-render"]).expect("valid CLI args");
        assert!(cli.debug_render);
    }

    #[test]
    fn parses_debug_render_log_flag() {
        let cli = Cli::try_parse_from(["bmux", "--debug-render-log", "render.log"])
            .expect("valid CLI args");
        assert_eq!(
            cli.debug_render_log.as_deref(),
            Some(std::path::Path::new("render.log"))
        );
    }

    #[test]
    fn parses_debug_render_log_csv_format() {
        let cli = Cli::try_parse_from([
            "bmux",
            "--debug-render-log",
            "render.log",
            "--debug-render-log-format",
            "csv",
        ])
        .expect("valid CLI args");
        assert_eq!(
            cli.debug_render_log_format,
            super::DebugRenderLogFormat::Csv
        );
    }

    #[test]
    fn parses_keymap_doctor_subcommand() {
        let cli = Cli::try_parse_from(["bmux", "keymap", "doctor"]).expect("valid CLI args");
        let Some(Command::Keymap { command }) = cli.command else {
            panic!("expected keymap subcommand");
        };
        assert!(matches!(command, KeymapCommand::Doctor { json: false }));
    }

    #[test]
    fn parses_keymap_doctor_json_flag() {
        let cli =
            Cli::try_parse_from(["bmux", "keymap", "doctor", "--json"]).expect("valid CLI args");
        let Some(Command::Keymap { command }) = cli.command else {
            panic!("expected keymap subcommand");
        };
        assert!(matches!(command, KeymapCommand::Doctor { json: true }));
    }

    #[test]
    fn parses_layout_clear_subcommand() {
        let cli = Cli::try_parse_from(["bmux", "layout", "clear"]).expect("valid CLI args");
        let Some(Command::Layout { command }) = cli.command else {
            panic!("expected layout subcommand");
        };
        assert!(matches!(command, LayoutCommand::Clear));
    }

    #[test]
    fn parses_server_start_default_foreground() {
        let cli = Cli::try_parse_from(["bmux", "server", "start"]).expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(
            command,
            ServerCommand::Start {
                daemon: false,
                foreground_internal: false
            }
        ));
    }

    #[test]
    fn parses_server_start_daemon_flag() {
        let cli =
            Cli::try_parse_from(["bmux", "server", "start", "--daemon"]).expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(
            command,
            ServerCommand::Start {
                daemon: true,
                foreground_internal: false
            }
        ));
    }

    #[test]
    fn parses_server_status_subcommand() {
        let cli = Cli::try_parse_from(["bmux", "server", "status"]).expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(command, ServerCommand::Status));
    }

    #[test]
    fn parses_server_stop_subcommand() {
        let cli = Cli::try_parse_from(["bmux", "server", "stop"]).expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(command, ServerCommand::Stop));
    }

    #[test]
    fn parses_top_level_new_session_command() {
        let cli = Cli::try_parse_from(["bmux", "new-session", "dev"]).expect("valid CLI args");
        let Some(Command::NewSession { name }) = cli.command else {
            panic!("expected new-session command");
        };
        assert_eq!(name.as_deref(), Some("dev"));
    }

    #[test]
    fn parses_top_level_list_sessions_command() {
        let cli = Cli::try_parse_from(["bmux", "list-sessions"]).expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::ListSessions { json: false })
        ));
    }

    #[test]
    fn parses_top_level_list_sessions_json_flag() {
        let cli = Cli::try_parse_from(["bmux", "list-sessions", "--json"]).expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::ListSessions { json: true })
        ));
    }

    #[test]
    fn parses_top_level_kill_session_command() {
        let cli = Cli::try_parse_from(["bmux", "kill-session", "dev"]).expect("valid CLI args");
        let Some(Command::KillSession { target }) = cli.command else {
            panic!("expected kill-session command");
        };
        assert_eq!(target, "dev");
    }

    #[test]
    fn parses_top_level_attach_command() {
        let cli = Cli::try_parse_from(["bmux", "attach", "dev"]).expect("valid CLI args");
        let Some(Command::Attach { target }) = cli.command else {
            panic!("expected attach command");
        };
        assert_eq!(target, "dev");
    }

    #[test]
    fn parses_top_level_detach_command() {
        let cli = Cli::try_parse_from(["bmux", "detach"]).expect("valid CLI args");
        assert!(matches!(cli.command, Some(Command::Detach)));
    }

    #[test]
    fn parses_grouped_session_new_command() {
        let cli = Cli::try_parse_from(["bmux", "session", "new", "dev"]).expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(
            command,
            SessionCommand::New { name: Some(ref name) } if name == "dev"
        ));
    }

    #[test]
    fn parses_grouped_session_list_command() {
        let cli = Cli::try_parse_from(["bmux", "session", "list"]).expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(command, SessionCommand::List { json: false }));
    }

    #[test]
    fn parses_grouped_session_list_json_flag() {
        let cli =
            Cli::try_parse_from(["bmux", "session", "list", "--json"]).expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(command, SessionCommand::List { json: true }));
    }

    #[test]
    fn parses_grouped_session_kill_command() {
        let cli = Cli::try_parse_from(["bmux", "session", "kill", "dev"]).expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(
            command,
            SessionCommand::Kill { target } if target == "dev"
        ));
    }

    #[test]
    fn parses_grouped_session_attach_command() {
        let cli =
            Cli::try_parse_from(["bmux", "session", "attach", "dev"]).expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(
            command,
            SessionCommand::Attach { target } if target == "dev"
        ));
    }

    #[test]
    fn parses_grouped_session_detach_command() {
        let cli = Cli::try_parse_from(["bmux", "session", "detach"]).expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(command, SessionCommand::Detach));
    }

    #[test]
    fn parses_terminal_doctor_subcommand() {
        let cli = Cli::try_parse_from(["bmux", "terminal", "doctor"]).expect("valid CLI args");
        let Some(Command::Terminal { command }) = cli.command else {
            panic!("expected terminal subcommand");
        };
        assert!(matches!(
            command,
            TerminalCommand::Doctor {
                json: false,
                trace: false,
                trace_limit: 50,
                trace_family: None,
                trace_pane: None
            }
        ));
    }

    #[test]
    fn parses_terminal_doctor_json_flag() {
        let cli =
            Cli::try_parse_from(["bmux", "terminal", "doctor", "--json"]).expect("valid CLI args");
        let Some(Command::Terminal { command }) = cli.command else {
            panic!("expected terminal subcommand");
        };
        assert!(matches!(
            command,
            TerminalCommand::Doctor {
                json: true,
                trace: false,
                trace_limit: 50,
                trace_family: None,
                trace_pane: None
            }
        ));
    }

    #[test]
    fn parses_terminal_doctor_trace_flags() {
        let cli = Cli::try_parse_from([
            "bmux",
            "terminal",
            "doctor",
            "--trace",
            "--trace-limit",
            "25",
        ])
        .expect("valid CLI args");
        let Some(Command::Terminal { command }) = cli.command else {
            panic!("expected terminal subcommand");
        };
        assert!(matches!(
            command,
            TerminalCommand::Doctor {
                json: false,
                trace: true,
                trace_limit: 25,
                trace_family: None,
                trace_pane: None
            }
        ));
    }

    #[test]
    fn parses_terminal_doctor_trace_filters() {
        let cli = Cli::try_parse_from([
            "bmux",
            "terminal",
            "doctor",
            "--trace",
            "--trace-family",
            "csi",
            "--trace-pane",
            "2",
        ])
        .expect("valid CLI args");
        let Some(Command::Terminal { command }) = cli.command else {
            panic!("expected terminal subcommand");
        };
        assert!(matches!(
            command,
            TerminalCommand::Doctor {
                json: false,
                trace: true,
                trace_limit: 50,
                trace_family: Some(TraceFamily::Csi),
                trace_pane: Some(2)
            }
        ));
    }

    #[test]
    fn parses_terminal_install_terminfo_flags() {
        let cli = Cli::try_parse_from(["bmux", "terminal", "install-terminfo", "--yes", "--check"])
            .expect("valid CLI args");
        let Some(Command::Terminal { command }) = cli.command else {
            panic!("expected terminal subcommand");
        };
        assert!(matches!(
            command,
            TerminalCommand::InstallTerminfo {
                yes: true,
                check: true
            }
        ));
    }
}
