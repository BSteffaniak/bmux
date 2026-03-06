use clap::{Parser, Subcommand, ValueEnum};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum DebugRenderLogFormat {
    Text,
    Csv,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum TraceFamily {
    Csi,
    Osc,
    Dcs,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RoleValue {
    Owner,
    Writer,
    Observer,
}

#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
#[command(name = "bmux")]
#[command(about = "Server-backed terminal multiplexer CLI")]
pub struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,

    /// Enable verbose logging
    #[arg(short, long)]
    pub(crate) verbose: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
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
    /// List connected clients
    ListClients {
        /// Print clients as JSON
        #[arg(long)]
        json: bool,
    },
    /// List explicit role assignments for a session
    Permissions {
        /// Session name or UUID
        #[arg(long)]
        session: String,
        /// Print permissions as JSON
        #[arg(long)]
        json: bool,
        /// Keep watching permission changes until interrupted
        #[arg(long, conflicts_with = "json")]
        watch: bool,
    },
    /// Grant a role to a client in a session
    Grant {
        /// Session name or UUID
        #[arg(long)]
        session: String,
        /// Target client UUID
        #[arg(long)]
        client: String,
        /// Role to grant
        #[arg(long, value_enum)]
        role: RoleValue,
    },
    /// Revoke explicit role from a client in a session
    Revoke {
        /// Session name or UUID
        #[arg(long)]
        session: String,
        /// Target client UUID
        #[arg(long)]
        client: String,
    },
    /// Kill a session by name or UUID
    KillSession {
        /// Session name or UUID
        target: String,
        /// Bypass ownership checks for local kill operations
        #[arg(long)]
        force_local: bool,
    },
    /// Kill all sessions
    KillAllSessions {
        /// Bypass ownership checks for local kill operations
        #[arg(long)]
        force_local: bool,
    },
    /// Attach to a session by name or UUID
    Attach {
        /// Session name or UUID
        target: Option<String>,
        /// Follow target client UUID and attach to its selected session
        #[arg(long)]
        follow: Option<String>,
        /// Keep following across target session switches (requires --follow)
        #[arg(long, requires = "follow")]
        global: bool,
    },
    /// Detach from the current session
    Detach,
    /// Create a new window in a session
    NewWindow {
        /// Optional session name or UUID (uses attached session when omitted)
        #[arg(long)]
        session: Option<String>,
        /// Optional window name
        #[arg(long)]
        name: Option<String>,
    },
    /// List windows for a session
    ListWindows {
        /// Optional session name or UUID (uses attached session when omitted)
        #[arg(long)]
        session: Option<String>,
        /// Print windows as JSON
        #[arg(long)]
        json: bool,
    },
    /// Kill a window by name, UUID, or `active`
    KillWindow {
        /// Window name, UUID, or `active`
        target: String,
        /// Optional session name or UUID (uses attached session when omitted)
        #[arg(long)]
        session: Option<String>,
        /// Bypass ownership checks for local kill operations
        #[arg(long)]
        force_local: bool,
    },
    /// Kill all windows in a session
    KillAllWindows {
        /// Optional session name or UUID (uses attached session when omitted)
        #[arg(long)]
        session: Option<String>,
        /// Bypass ownership checks for local kill operations
        #[arg(long)]
        force_local: bool,
    },
    /// Switch active window by name, UUID, or `active`
    SwitchWindow {
        /// Window name, UUID, or `active`
        target: String,
        /// Optional session name or UUID (uses attached session when omitted)
        #[arg(long)]
        session: Option<String>,
    },
    /// Follow another client's active target
    Follow {
        /// Target client UUID to follow
        target_client_id: String,
        /// Keep following across target session switches
        #[arg(long)]
        global: bool,
    },
    /// Stop following a client
    Unfollow,
    /// Session management commands
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Window management commands
    Window {
        #[command(subcommand)]
        command: WindowCommand,
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
    /// Terminal capability tools and diagnostics
    Terminal {
        #[command(subcommand)]
        command: TerminalCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum SessionCommand {
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
    /// List connected clients
    Clients {
        /// Print clients as JSON
        #[arg(long)]
        json: bool,
    },
    /// List explicit role assignments for a session
    Permissions {
        /// Session name or UUID
        #[arg(long)]
        session: String,
        /// Print permissions as JSON
        #[arg(long)]
        json: bool,
        /// Keep watching permission changes until interrupted
        #[arg(long, conflicts_with = "json")]
        watch: bool,
    },
    /// Grant a role to a client in a session
    Grant {
        /// Session name or UUID
        #[arg(long)]
        session: String,
        /// Target client UUID
        #[arg(long)]
        client: String,
        /// Role to grant
        #[arg(long, value_enum)]
        role: RoleValue,
    },
    /// Revoke explicit role from a client in a session
    Revoke {
        /// Session name or UUID
        #[arg(long)]
        session: String,
        /// Target client UUID
        #[arg(long)]
        client: String,
    },
    /// Kill a session by name or UUID
    Kill {
        /// Session name or UUID
        target: String,
        /// Bypass ownership checks for local kill operations
        #[arg(long)]
        force_local: bool,
    },
    /// Kill all sessions
    KillAll {
        /// Bypass ownership checks for local kill operations
        #[arg(long)]
        force_local: bool,
    },
    /// Attach to a session by name or UUID
    Attach {
        /// Session name or UUID
        target: Option<String>,
        /// Follow target client UUID and attach to its selected session
        #[arg(long)]
        follow: Option<String>,
        /// Keep following across target session switches (requires --follow)
        #[arg(long, requires = "follow")]
        global: bool,
    },
    /// Detach from the current session
    Detach,
    /// Follow another client's active target
    Follow {
        /// Target client UUID to follow
        target_client_id: String,
        /// Keep following across target session switches
        #[arg(long)]
        global: bool,
    },
    /// Stop following a client
    Unfollow,
}

#[derive(Debug, Subcommand)]
pub enum WindowCommand {
    /// Create a new window in a session
    New {
        /// Optional session name or UUID (uses attached session when omitted)
        #[arg(long)]
        session: Option<String>,
        /// Optional window name
        #[arg(long)]
        name: Option<String>,
    },
    /// List windows for a session
    List {
        /// Optional session name or UUID (uses attached session when omitted)
        #[arg(long)]
        session: Option<String>,
        /// Print windows as JSON
        #[arg(long)]
        json: bool,
    },
    /// Kill a window by name, UUID, or `active`
    Kill {
        /// Window name, UUID, or `active`
        target: String,
        /// Optional session name or UUID (uses attached session when omitted)
        #[arg(long)]
        session: Option<String>,
        /// Bypass ownership checks for local kill operations
        #[arg(long)]
        force_local: bool,
    },
    /// Kill all windows in a session
    KillAll {
        /// Optional session name or UUID (uses attached session when omitted)
        #[arg(long)]
        session: Option<String>,
        /// Bypass ownership checks for local kill operations
        #[arg(long)]
        force_local: bool,
    },
    /// Switch active window by name, UUID, or `active`
    Switch {
        /// Window name, UUID, or `active`
        target: String,
        /// Optional session name or UUID (uses attached session when omitted)
        #[arg(long)]
        session: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ServerCommand {
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
    Status {
        /// Print server status as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show caller and server owner principal identities
    WhoamiPrincipal {
        /// Print principal identity as JSON
        #[arg(long)]
        json: bool,
    },
    /// Trigger immediate server snapshot save
    Save,
    /// Validate persisted snapshot without applying restore
    Restore {
        /// Only validate snapshot readability and schema
        #[arg(long)]
        dry_run: bool,
        /// Confirm replace-restore of current in-memory server state
        #[arg(long, conflicts_with = "dry_run")]
        yes: bool,
    },
    /// Request graceful server shutdown
    Stop,
}

#[derive(Debug, Subcommand)]
pub enum KeymapCommand {
    /// Print compiled keymap and overlap diagnostics
    Doctor {
        /// Print diagnostics as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum TerminalCommand {
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
        Cli, Command, KeymapCommand, RoleValue, ServerCommand, SessionCommand, TerminalCommand,
        TraceFamily, WindowCommand,
    };
    use clap::Parser;

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
        assert!(matches!(command, ServerCommand::Status { json: false }));
    }

    #[test]
    fn parses_server_status_json_flag() {
        let cli =
            Cli::try_parse_from(["bmux", "server", "status", "--json"]).expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(command, ServerCommand::Status { json: true }));
    }

    #[test]
    fn parses_server_whoami_principal_subcommand() {
        let cli =
            Cli::try_parse_from(["bmux", "server", "whoami-principal"]).expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(
            command,
            ServerCommand::WhoamiPrincipal { json: false }
        ));
    }

    #[test]
    fn parses_server_whoami_principal_json_flag() {
        let cli = Cli::try_parse_from(["bmux", "server", "whoami-principal", "--json"])
            .expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(
            command,
            ServerCommand::WhoamiPrincipal { json: true }
        ));
    }

    #[test]
    fn parses_server_save_subcommand() {
        let cli = Cli::try_parse_from(["bmux", "server", "save"]).expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(command, ServerCommand::Save));
    }

    #[test]
    fn parses_server_restore_dry_run_subcommand() {
        let cli = Cli::try_parse_from(["bmux", "server", "restore", "--dry-run"])
            .expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(
            command,
            ServerCommand::Restore {
                dry_run: true,
                yes: false
            }
        ));
    }

    #[test]
    fn parses_server_restore_yes_subcommand() {
        let cli =
            Cli::try_parse_from(["bmux", "server", "restore", "--yes"]).expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(
            command,
            ServerCommand::Restore {
                dry_run: false,
                yes: true
            }
        ));
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
        let Some(Command::KillSession {
            target,
            force_local,
        }) = cli.command
        else {
            panic!("expected kill-session command");
        };
        assert_eq!(target, "dev");
        assert!(!force_local);
    }

    #[test]
    fn parses_top_level_kill_session_force_local_flag() {
        let cli = Cli::try_parse_from(["bmux", "kill-session", "dev", "--force-local"])
            .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::KillSession {
                target,
                force_local: true
            }) if target == "dev"
        ));
    }

    #[test]
    fn parses_top_level_kill_all_sessions_command() {
        let cli = Cli::try_parse_from(["bmux", "kill-all-sessions"]).expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::KillAllSessions { force_local: false })
        ));
    }

    #[test]
    fn parses_top_level_kill_all_sessions_force_local_flag() {
        let cli = Cli::try_parse_from(["bmux", "kill-all-sessions", "--force-local"])
            .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::KillAllSessions { force_local: true })
        ));
    }

    #[test]
    fn parses_top_level_attach_command() {
        let cli = Cli::try_parse_from(["bmux", "attach", "dev"]).expect("valid CLI args");
        let Some(Command::Attach {
            target,
            follow,
            global,
        }) = cli.command
        else {
            panic!("expected attach command");
        };
        assert_eq!(target.as_deref(), Some("dev"));
        assert_eq!(follow, None);
        assert!(!global);
    }

    #[test]
    fn parses_top_level_attach_follow_command() {
        let cli = Cli::try_parse_from([
            "bmux",
            "attach",
            "--follow",
            "550e8400-e29b-41d4-a716-446655440000",
            "--global",
        ])
        .expect("valid CLI args");
        let Some(Command::Attach {
            target,
            follow,
            global,
        }) = cli.command
        else {
            panic!("expected attach command");
        };
        assert_eq!(target, None);
        assert_eq!(
            follow.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert!(global);
    }

    #[test]
    fn parses_top_level_detach_command() {
        let cli = Cli::try_parse_from(["bmux", "detach"]).expect("valid CLI args");
        assert!(matches!(cli.command, Some(Command::Detach)));
    }

    #[test]
    fn parses_top_level_new_window_command() {
        let cli =
            Cli::try_parse_from(["bmux", "new-window", "--name", "editor", "--session", "dev"])
                .expect("valid CLI args");
        let Some(Command::NewWindow { session, name }) = cli.command else {
            panic!("expected new-window command");
        };
        assert_eq!(session.as_deref(), Some("dev"));
        assert_eq!(name.as_deref(), Some("editor"));
    }

    #[test]
    fn parses_top_level_list_windows_json_flag() {
        let cli = Cli::try_parse_from(["bmux", "list-windows", "--json"]).expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::ListWindows {
                session: None,
                json: true
            })
        ));
    }

    #[test]
    fn parses_top_level_kill_window_command() {
        let cli = Cli::try_parse_from(["bmux", "kill-window", "active", "--session", "dev"])
            .expect("valid CLI args");
        let Some(Command::KillWindow {
            target,
            session,
            force_local,
        }) = cli.command
        else {
            panic!("expected kill-window command");
        };
        assert_eq!(target, "active");
        assert_eq!(session.as_deref(), Some("dev"));
        assert!(!force_local);
    }

    #[test]
    fn parses_top_level_kill_all_windows_command() {
        let cli = Cli::try_parse_from(["bmux", "kill-all-windows", "--session", "dev"])
            .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::KillAllWindows {
                session: Some(ref session),
                force_local: false
            }) if session == "dev"
        ));
    }

    #[test]
    fn parses_top_level_kill_all_windows_without_session_command() {
        let cli = Cli::try_parse_from(["bmux", "kill-all-windows"]).expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::KillAllWindows {
                session: None,
                force_local: false
            })
        ));
    }

    #[test]
    fn parses_top_level_kill_all_windows_force_local_flag() {
        let cli = Cli::try_parse_from(["bmux", "kill-all-windows", "--force-local"])
            .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::KillAllWindows {
                session: None,
                force_local: true
            })
        ));
    }

    #[test]
    fn parses_top_level_switch_window_command() {
        let cli = Cli::try_parse_from(["bmux", "switch-window", "editor"]).expect("valid CLI args");
        let Some(Command::SwitchWindow { target, session }) = cli.command else {
            panic!("expected switch-window command");
        };
        assert_eq!(target, "editor");
        assert_eq!(session, None);
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
    fn parses_top_level_list_clients_command() {
        let cli = Cli::try_parse_from(["bmux", "list-clients"]).expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::ListClients { json: false })
        ));
    }

    #[test]
    fn parses_top_level_list_clients_json_flag() {
        let cli = Cli::try_parse_from(["bmux", "list-clients", "--json"]).expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::ListClients { json: true })
        ));
    }

    #[test]
    fn parses_top_level_permissions_command() {
        let cli = Cli::try_parse_from(["bmux", "permissions", "--session", "dev"])
            .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::Permissions {
                session,
                json: false,
                watch: false
            }) if session == "dev"
        ));
    }

    #[test]
    fn parses_top_level_permissions_watch_command() {
        let cli = Cli::try_parse_from(["bmux", "permissions", "--session", "dev", "--watch"])
            .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::Permissions {
                session,
                json: false,
                watch: true
            }) if session == "dev"
        ));
    }

    #[test]
    fn parses_top_level_grant_command() {
        let cli = Cli::try_parse_from([
            "bmux",
            "grant",
            "--session",
            "dev",
            "--client",
            "550e8400-e29b-41d4-a716-446655440000",
            "--role",
            "writer",
        ])
        .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::Grant {
                session,
                client,
                role: RoleValue::Writer
            }) if session == "dev" && client == "550e8400-e29b-41d4-a716-446655440000"
        ));
    }

    #[test]
    fn parses_top_level_revoke_command() {
        let cli = Cli::try_parse_from([
            "bmux",
            "revoke",
            "--session",
            "dev",
            "--client",
            "550e8400-e29b-41d4-a716-446655440000",
        ])
        .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::Revoke { session, client })
                if session == "dev" && client == "550e8400-e29b-41d4-a716-446655440000"
        ));
    }

    #[test]
    fn parses_grouped_session_clients_command() {
        let cli = Cli::try_parse_from(["bmux", "session", "clients"]).expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(command, SessionCommand::Clients { json: false }));
    }

    #[test]
    fn parses_grouped_session_clients_json_flag() {
        let cli =
            Cli::try_parse_from(["bmux", "session", "clients", "--json"]).expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(command, SessionCommand::Clients { json: true }));
    }

    #[test]
    fn parses_grouped_session_permissions_command() {
        let cli = Cli::try_parse_from(["bmux", "session", "permissions", "--session", "dev"])
            .expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(
            command,
            SessionCommand::Permissions {
                session,
                json: false,
                watch: false
            } if session == "dev"
        ));
    }

    #[test]
    fn parses_grouped_session_permissions_watch_command() {
        let cli = Cli::try_parse_from([
            "bmux",
            "session",
            "permissions",
            "--session",
            "dev",
            "--watch",
        ])
        .expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(
            command,
            SessionCommand::Permissions {
                session,
                json: false,
                watch: true
            } if session == "dev"
        ));
    }

    #[test]
    fn parses_grouped_session_grant_command() {
        let cli = Cli::try_parse_from([
            "bmux",
            "session",
            "grant",
            "--session",
            "dev",
            "--client",
            "550e8400-e29b-41d4-a716-446655440000",
            "--role",
            "observer",
        ])
        .expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(
            command,
            SessionCommand::Grant {
                session,
                client,
                role: RoleValue::Observer
            } if session == "dev" && client == "550e8400-e29b-41d4-a716-446655440000"
        ));
    }

    #[test]
    fn parses_grouped_session_revoke_command() {
        let cli = Cli::try_parse_from([
            "bmux",
            "session",
            "revoke",
            "--session",
            "dev",
            "--client",
            "550e8400-e29b-41d4-a716-446655440000",
        ])
        .expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(
            command,
            SessionCommand::Revoke { session, client }
                if session == "dev" && client == "550e8400-e29b-41d4-a716-446655440000"
        ));
    }

    #[test]
    fn parses_grouped_session_kill_command() {
        let cli = Cli::try_parse_from(["bmux", "session", "kill", "dev"]).expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(
            command,
            SessionCommand::Kill {
                target,
                force_local: false
            } if target == "dev"
        ));
    }

    #[test]
    fn parses_grouped_session_kill_all_command() {
        let cli = Cli::try_parse_from(["bmux", "session", "kill-all"]).expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(
            command,
            SessionCommand::KillAll { force_local: false }
        ));
    }

    #[test]
    fn parses_grouped_session_kill_all_force_local_flag() {
        let cli = Cli::try_parse_from(["bmux", "session", "kill-all", "--force-local"])
            .expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(
            command,
            SessionCommand::KillAll { force_local: true }
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
            SessionCommand::Attach {
                target: Some(target),
                follow: None,
                global: false
            } if target == "dev"
        ));
    }

    #[test]
    fn parses_grouped_session_attach_follow_command() {
        let cli = Cli::try_parse_from([
            "bmux",
            "session",
            "attach",
            "--follow",
            "550e8400-e29b-41d4-a716-446655440000",
            "--global",
        ])
        .expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(
            command,
            SessionCommand::Attach {
                target: None,
                follow: Some(ref follow),
                global: true
            } if follow == "550e8400-e29b-41d4-a716-446655440000"
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
    fn parses_top_level_follow_command() {
        let cli = Cli::try_parse_from([
            "bmux",
            "follow",
            "550e8400-e29b-41d4-a716-446655440000",
            "--global",
        ])
        .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::Follow {
                ref target_client_id,
                global: true
            }) if target_client_id == "550e8400-e29b-41d4-a716-446655440000"
        ));
    }

    #[test]
    fn parses_top_level_unfollow_command() {
        let cli = Cli::try_parse_from(["bmux", "unfollow"]).expect("valid CLI args");
        assert!(matches!(cli.command, Some(Command::Unfollow)));
    }

    #[test]
    fn parses_grouped_session_follow_command() {
        let cli = Cli::try_parse_from([
            "bmux",
            "session",
            "follow",
            "550e8400-e29b-41d4-a716-446655440000",
            "--global",
        ])
        .expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(
            command,
            SessionCommand::Follow {
                ref target_client_id,
                global: true
            } if target_client_id == "550e8400-e29b-41d4-a716-446655440000"
        ));
    }

    #[test]
    fn parses_grouped_session_unfollow_command() {
        let cli = Cli::try_parse_from(["bmux", "session", "unfollow"]).expect("valid CLI args");
        let Some(Command::Session { command }) = cli.command else {
            panic!("expected session command");
        };
        assert!(matches!(command, SessionCommand::Unfollow));
    }

    #[test]
    fn parses_grouped_window_new_command() {
        let cli = Cli::try_parse_from([
            "bmux",
            "window",
            "new",
            "--session",
            "dev",
            "--name",
            "editor",
        ])
        .expect("valid CLI args");
        let Some(Command::Window { command }) = cli.command else {
            panic!("expected window command");
        };
        assert!(matches!(
            command,
            WindowCommand::New {
                session: Some(ref session),
                name: Some(ref name)
            } if session == "dev" && name == "editor"
        ));
    }

    #[test]
    fn parses_grouped_window_list_command() {
        let cli = Cli::try_parse_from(["bmux", "window", "list"]).expect("valid CLI args");
        let Some(Command::Window { command }) = cli.command else {
            panic!("expected window command");
        };
        assert!(matches!(
            command,
            WindowCommand::List {
                session: None,
                json: false
            }
        ));
    }

    #[test]
    fn parses_grouped_window_kill_command() {
        let cli = Cli::try_parse_from(["bmux", "window", "kill", "active", "--session", "dev"])
            .expect("valid CLI args");
        let Some(Command::Window { command }) = cli.command else {
            panic!("expected window command");
        };
        assert!(matches!(
            command,
            WindowCommand::Kill {
                target,
                session: Some(ref session),
                force_local: false
            } if target == "active" && session == "dev"
        ));
    }

    #[test]
    fn parses_grouped_window_kill_force_local_flag() {
        let cli = Cli::try_parse_from([
            "bmux",
            "window",
            "kill",
            "active",
            "--session",
            "dev",
            "--force-local",
        ])
        .expect("valid CLI args");
        let Some(Command::Window { command }) = cli.command else {
            panic!("expected window command");
        };
        assert!(matches!(
            command,
            WindowCommand::Kill {
                target,
                session: Some(ref session),
                force_local: true
            } if target == "active" && session == "dev"
        ));
    }

    #[test]
    fn parses_grouped_window_kill_all_command() {
        let cli = Cli::try_parse_from(["bmux", "window", "kill-all", "--session", "dev"])
            .expect("valid CLI args");
        let Some(Command::Window { command }) = cli.command else {
            panic!("expected window command");
        };
        assert!(matches!(
            command,
            WindowCommand::KillAll {
                session: Some(ref session),
                force_local: false
            } if session == "dev"
        ));
    }

    #[test]
    fn parses_grouped_window_kill_all_without_session_command() {
        let cli = Cli::try_parse_from(["bmux", "window", "kill-all"]).expect("valid CLI args");
        let Some(Command::Window { command }) = cli.command else {
            panic!("expected window command");
        };
        assert!(matches!(
            command,
            WindowCommand::KillAll {
                session: None,
                force_local: false
            }
        ));
    }

    #[test]
    fn parses_grouped_window_switch_command() {
        let cli =
            Cli::try_parse_from(["bmux", "window", "switch", "main"]).expect("valid CLI args");
        let Some(Command::Window { command }) = cli.command else {
            panic!("expected window command");
        };
        assert!(matches!(
            command,
            WindowCommand::Switch { target, session: None } if target == "main"
        ));
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
