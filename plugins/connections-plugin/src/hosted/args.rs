use super::{
    HostedClient, HostedMode, Result, connect, run_auth_login, run_auth_logout, run_auth_status,
    run_host_status, run_host_stop, run_hosts, run_join, run_setup, run_share, run_unshare,
};
use clap::{Parser, Subcommand, ValueEnum};
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    P2p,
    ControlPlane,
}
impl From<Mode> for HostedMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::P2p => Self::P2p,
            Mode::ControlPlane => Self::ControlPlane,
        }
    }
}
#[derive(Debug, Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Debug, Subcommand)]
enum Command {
    /// First-run setup wizard for hosted mode (p2p default)
    Setup {
        /// Check hosted readiness without starting or changing runtime state
        #[arg(long)]
        check: bool,
        /// Hosted operation mode (`p2p` is infra-light default)
        #[arg(long, value_enum)]
        mode: Option<Mode>,
    },
    /// Start hosted mode (p2p default, control-plane opt-in)
    Host {
        /// Optional listen address for local gateway bridge
        #[arg(long, default_value = "127.0.0.1:7443")]
        listen: String,
        /// Optional friendly name hint
        #[arg(long)]
        name: Option<String>,
        /// Copy the resulting join link to clipboard
        #[arg(long)]
        copy: bool,
        /// Run host runtime in the background
        #[arg(long, conflicts_with_all = ["status", "stop", "restart"])]
        daemon: bool,
        /// Show hosted-mode runtime status
        #[arg(long, conflicts_with_all = ["stop", "restart", "daemon"])]
        status: bool,
        /// Stop hosted-mode runtime if running
        #[arg(long, conflicts_with_all = ["status", "restart", "daemon"])]
        stop: bool,
        /// Restart hosted-mode runtime in background
        #[arg(long, conflicts_with_all = ["status", "stop", "daemon"])]
        restart: bool,
        /// Hosted operation mode (`p2p` is infra-light default)
        #[arg(long, value_enum)]
        mode: Option<Mode>,
    },
    /// Join a hosted link/target quickly
    Join {
        /// Link or target name (bmux://, iroh://, https://, or configured target)
        link: Option<String>,
        /// Session name or UUID
        session: Option<String>,
    },
    /// List known hosts/targets (recent first)
    Hosts {
        /// Include detailed target mappings and diagnostics
        #[arg(long)]
        verbose: bool,
    },
    /// Authentication commands
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Share helpers for hosted links
    Share {
        /// Target/link to share
        target: Option<String>,
        /// Optional second positional argument (used by `bmux share revoke <name>`)
        secondary: Option<String>,
        /// Optional stable share name
        #[arg(long)]
        name: Option<String>,
        /// Optional access role hint
        #[arg(long, default_value = "control")]
        role: String,
        /// Optional invite TTL (example: 24h, 30m)
        #[arg(long)]
        ttl: Option<String>,
        /// Mark the share as one-time use
        #[arg(long)]
        one_time: bool,
        /// Copy resulting share link to clipboard
        #[arg(long)]
        copy: bool,
        /// Render a terminal QR code for the share link
        #[arg(long)]
        qr: bool,
    },
    /// Remove a named shared link
    Unshare {
        /// Share name/slug to remove
        name: String,
    },
    /// Connect to a target and attach to a session
    Connect {
        /// Target name or ssh destination (user@host[:port] or ssh://...)
        target: Option<String>,
        /// Session name or UUID; if omitted in TTY mode a picker is shown
        session: Option<String>,
        /// Follow target client UUID and attach to its selected session
        #[arg(long, conflicts_with = "session")]
        follow: Option<String>,
        /// Keep following across target session switches (requires --follow)
        #[arg(long, requires = "follow")]
        global: bool,
        /// Keep reconnecting instead of stopping after bounded retries
        #[arg(long)]
        reconnect_forever: bool,
    },
}
#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Login and register this device for hosted mode
    Login {
        /// Do not try to open a browser automatically
        #[arg(long)]
        no_browser: bool,
    },
    /// Show current authentication state
    Status,
    /// Clear locally stored authentication state
    Logout,
}
impl HostedClient {
    pub(crate) fn run(&self, context: &bmux_plugin_sdk::NativeCommandContext) -> Result<i32> {
        anyhow::ensure!(
            context.invocation_source
                == bmux_plugin_sdk::NativeCommandInvocationSource::LocalProcess
                && context.caller_client_id.is_none(),
            "hosted commands require a standalone local client"
        );
        let mut tokens = vec!["bmux".to_owned()];
        tokens.extend(context.command.split('-').map(str::to_owned));
        tokens.extend(context.arguments.iter().cloned());
        let args = match Args::try_parse_from(tokens) {
            Ok(args) => args,
            Err(error) => {
                error.print()?;
                return Ok(error.exit_code());
            }
        };
        crate::block_on_connection(async {match args.command {
            Command::Auth {command: AuthCommand::Login {no_browser}} => run_auth_login(self,no_browser).await,
            Command::Auth {command: AuthCommand::Status} => run_auth_status(self),
            Command::Auth {command: AuthCommand::Logout} => run_auth_logout(self),
            Command::Share {target,secondary,name,role,ttl,one_time,copy,qr} => run_share(self,target.as_deref(),secondary.as_deref(),name.as_deref(),&role,ttl.as_deref(),one_time,copy,qr).await,
            Command::Unshare {name} => run_unshare(self,&name).await,
            Command::Host {status:true,..} => run_host_status(self),
            Command::Host {stop:true,..} => run_host_stop(self),
            // Legacy ingress is unconditionally denied before any listener, restart,
            // copy, or daemon effect. Do not reintroduce the pre-admission listener.
            Command::Host {..} => anyhow::bail!("legacy Iroh host is disabled: legacy remote ingress cannot establish trusted stream admission; use the connections plugin admission protocol (no downgrade)"),
            Command::Hosts {verbose} => run_hosts(self,verbose),
            Command::Setup {check,mode} => run_setup(self,check,mode.map(Into::into)).await,
            Command::Join {link,session} => run_join(self,link.as_deref(),session.as_deref()).await,
            Command::Connect {target,session,follow,global,reconnect_forever} => connect(self,target.as_deref(),session.as_deref(),follow.as_deref(),global,reconnect_forever).await,
        }}).map(i32::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_connect_command_with_session() {
        let cli = Args::try_parse_from(["bmux", "connect", "prod", "app"]).expect("valid CLI args");
        let Some(Command::Connect {
            target,
            session,
            follow,
            global,
            reconnect_forever,
        }) = Some(cli.command)
        else {
            panic!("expected connect command");
        };
        assert_eq!(target.as_deref(), Some("prod"));
        assert_eq!(session.as_deref(), Some("app"));
        assert!(follow.is_none());
        assert!(!global);
        assert!(!reconnect_forever);
    }

    #[test]
    fn parses_setup_command_defaults() {
        let cli = Args::try_parse_from(["bmux", "setup"]).expect("valid CLI args");
        let Some(Command::Setup { check, mode }) = Some(cli.command) else {
            panic!("expected setup command");
        };
        assert!(!check);
        assert!(mode.is_none());
    }

    #[test]
    fn parses_setup_check_flag() {
        let cli = Args::try_parse_from(["bmux", "setup", "--check"]).expect("valid CLI args");
        let Some(Command::Setup { check, mode }) = Some(cli.command) else {
            panic!("expected setup command");
        };
        assert!(check);
        assert!(mode.is_none());
    }

    #[test]
    fn parses_setup_mode_flag() {
        let cli = Args::try_parse_from(["bmux", "setup", "--mode", "control-plane"])
            .expect("valid CLI args");
        let Some(Command::Setup { mode, .. }) = Some(cli.command) else {
            panic!("expected setup command");
        };
        assert_eq!(mode, Some(Mode::ControlPlane));
    }

    #[test]
    fn parses_hosts_defaults() {
        let cli = Args::try_parse_from(["bmux", "hosts"]).expect("valid CLI args");
        let Some(Command::Hosts { verbose }) = Some(cli.command) else {
            panic!("expected hosts command");
        };
        assert!(!verbose);
    }

    #[test]
    fn parses_hosts_verbose_flag() {
        let cli = Args::try_parse_from(["bmux", "hosts", "--verbose"]).expect("valid CLI args");
        let Some(Command::Hosts { verbose }) = Some(cli.command) else {
            panic!("expected hosts command");
        };
        assert!(verbose);
    }

    #[test]
    fn parses_host_command_defaults() {
        let cli = Args::try_parse_from(["bmux", "host"]).expect("valid CLI args");
        let Some(Command::Host {
            listen,
            name,
            copy,
            daemon,
            status,
            stop,
            restart,
            mode,
        }) = Some(cli.command)
        else {
            panic!("expected host command");
        };
        assert_eq!(listen, "127.0.0.1:7443");
        assert!(name.is_none());
        assert!(!copy);
        assert!(!daemon);
        assert!(!status);
        assert!(!stop);
        assert!(!restart);
        assert!(mode.is_none());
    }

    #[test]
    fn parses_host_copy_flag() {
        let cli = Args::try_parse_from(["bmux", "host", "--copy"]).expect("valid CLI args");
        let Some(Command::Host { copy, .. }) = Some(cli.command) else {
            panic!("expected host command");
        };
        assert!(copy);
    }

    #[test]
    fn parses_host_status_flag() {
        let cli = Args::try_parse_from(["bmux", "host", "--status"]).expect("valid CLI args");
        let Some(Command::Host { status, .. }) = Some(cli.command) else {
            panic!("expected host command");
        };
        assert!(status);
    }

    #[test]
    fn parses_host_stop_flag() {
        let cli = Args::try_parse_from(["bmux", "host", "--stop"]).expect("valid CLI args");
        let Some(Command::Host { stop, .. }) = Some(cli.command) else {
            panic!("expected host command");
        };
        assert!(stop);
    }

    #[test]
    fn parses_host_daemon_flag() {
        let cli = Args::try_parse_from(["bmux", "host", "--daemon"]).expect("valid CLI args");
        let Some(Command::Host { daemon, .. }) = Some(cli.command) else {
            panic!("expected host command");
        };
        assert!(daemon);
    }

    #[test]
    fn parses_host_restart_flag() {
        let cli = Args::try_parse_from(["bmux", "host", "--restart"]).expect("valid CLI args");
        let Some(Command::Host { restart, .. }) = Some(cli.command) else {
            panic!("expected host command");
        };
        assert!(restart);
    }

    #[test]
    fn parses_host_mode_flag() {
        let cli = Args::try_parse_from(["bmux", "host", "--mode", "p2p"]).expect("valid CLI args");
        let Some(Command::Host { mode, .. }) = Some(cli.command) else {
            panic!("expected host command");
        };
        assert_eq!(mode, Some(Mode::P2p));
    }

    #[test]
    fn parses_auth_login_no_browser_flag() {
        let cli = Args::try_parse_from(["bmux", "auth", "login", "--no-browser"])
            .expect("valid CLI args");
        let Some(Command::Auth { command }) = Some(cli.command) else {
            panic!("expected auth command");
        };
        assert!(matches!(command, AuthCommand::Login { no_browser: true }));
    }

    #[test]
    fn parses_auth_status_command() {
        let cli = Args::try_parse_from(["bmux", "auth", "status"]).expect("valid CLI args");
        let Some(Command::Auth { command }) = Some(cli.command) else {
            panic!("expected auth command");
        };
        assert!(matches!(command, AuthCommand::Status));
    }

    #[test]
    fn parses_auth_logout_command() {
        let cli = Args::try_parse_from(["bmux", "auth", "logout"]).expect("valid CLI args");
        let Some(Command::Auth { command }) = Some(cli.command) else {
            panic!("expected auth command");
        };
        assert!(matches!(command, AuthCommand::Logout));
    }

    #[test]
    fn parses_share_flags() {
        let cli = Args::try_parse_from([
            "bmux",
            "share",
            "--name",
            "demo",
            "--role",
            "view",
            "--ttl",
            "24h",
            "--one-time",
            "--copy",
        ])
        .expect("valid CLI args");
        let Some(Command::Share {
            target,
            secondary,
            name,
            role,
            ttl,
            one_time,
            copy,
            qr,
        }) = Some(cli.command)
        else {
            panic!("expected share command");
        };
        assert!(target.is_none());
        assert!(secondary.is_none());
        assert_eq!(name.as_deref(), Some("demo"));
        assert_eq!(role, "view");
        assert_eq!(ttl.as_deref(), Some("24h"));
        assert!(one_time);
        assert!(copy);
        assert!(!qr);
    }

    #[test]
    fn parses_share_revoke_alias_style() {
        let cli =
            Args::try_parse_from(["bmux", "share", "revoke", "demo"]).expect("valid CLI args");
        let Some(Command::Share {
            target, secondary, ..
        }) = Some(cli.command)
        else {
            panic!("expected share command");
        };
        assert_eq!(target.as_deref(), Some("revoke"));
        assert_eq!(secondary.as_deref(), Some("demo"));
    }

    #[test]
    fn parses_share_qr_flag() {
        let cli = Args::try_parse_from(["bmux", "share", "--qr"]).expect("valid CLI args");
        let Some(Command::Share { qr, .. }) = Some(cli.command) else {
            panic!("expected share command");
        };
        assert!(qr);
    }

    #[test]
    fn parses_join_command_with_link() {
        let cli =
            Args::try_parse_from(["bmux", "join", "bmux://demo", "main"]).expect("valid CLI args");
        let Some(Command::Join { link, session }) = Some(cli.command) else {
            panic!("expected join command");
        };
        assert_eq!(link.as_deref(), Some("bmux://demo"));
        assert_eq!(session.as_deref(), Some("main"));
    }
}
