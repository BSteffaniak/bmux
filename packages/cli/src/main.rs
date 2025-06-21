#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]

//! Main CLI application for bmux terminal multiplexer

use anyhow::Result;
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_session::SessionId;
use clap::{Parser, Subcommand};
use tracing::info;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(name = "bmux")]
#[command(about = "A modern terminal multiplexer written in Rust")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Configuration file path
    #[arg(short, long)]
    config: Option<std::path::PathBuf>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Start a new session or attach to existing one
    #[command(alias = "new")]
    NewSession {
        /// Session name
        #[arg(short, long)]
        session: Option<String>,

        /// Detach after creating
        #[arg(short, long)]
        detach: bool,
    },

    /// Attach to an existing session
    #[command(alias = "a")]
    Attach {
        /// Session name or ID
        #[arg(short, long)]
        target: Option<String>,

        /// Create independent view
        #[arg(long)]
        independent: bool,

        /// Follow another client
        #[arg(long)]
        follow_client: Option<String>,
    },

    /// List all sessions
    #[command(alias = "ls")]
    List,

    /// Kill a session
    #[command(alias = "kill")]
    KillSession {
        /// Session name or ID
        target: String,
    },

    /// Show server information
    Info,

    /// Start the bmux server
    Server {
        /// Server socket path
        #[arg(short, long)]
        socket: Option<std::path::PathBuf>,

        /// Run in foreground
        #[arg(short, long)]
        foreground: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    init_logging(cli.verbose);

    // Load configuration
    let config = load_config(cli.config.as_deref())?;

    // Ensure config directories exist
    let config_paths = ConfigPaths::default();
    config_paths.ensure_dirs()?;

    info!("Starting bmux terminal multiplexer");

    match cli.command {
        Some(command) => execute_command(command, config),
        None => {
            // Default behavior: try to attach to a session, or create a new one
            default_action(config)
        }
    }
}

/// Initialize logging based on verbosity level
fn init_logging(verbose: bool) {
    #[cfg(feature = "logging")]
    {
        let level = if verbose {
            tracing::Level::DEBUG
        } else {
            tracing::Level::INFO
        };

        tracing_subscriber::fmt()
            .with_max_level(level)
            .with_target(false)
            .init();
    }

    #[cfg(not(feature = "logging"))]
    {
        let _ = verbose; // Silence unused variable warning
        println!("Logging disabled - compile with --features logging to enable");
    }
}

/// Load configuration from file or use defaults
fn load_config(config_path: Option<&std::path::Path>) -> Result<BmuxConfig> {
    let config = if let Some(path) = config_path {
        BmuxConfig::load_from_path(path)?
    } else {
        BmuxConfig::load()?
    };

    info!("Configuration loaded successfully");
    Ok(config)
}

/// Execute a specific command
#[allow(clippy::unnecessary_wraps)]
fn execute_command(command: Commands, _config: BmuxConfig) -> Result<()> {
    match command {
        Commands::NewSession { session, detach } => {
            info!("Creating new session: {:?}", session);
            create_new_session(session, detach);
            Ok(())
        }
        Commands::Attach {
            target,
            independent,
            follow_client,
        } => {
            info!("Attaching to session: {:?}", target);
            attach_to_session(target, independent, follow_client);
            Ok(())
        }
        Commands::List => {
            info!("Listing sessions");
            list_sessions();
            Ok(())
        }
        Commands::KillSession { target } => {
            info!("Killing session: {target}");
            kill_session(&target);
            Ok(())
        }
        Commands::Info => {
            info!("Showing server info");
            show_server_info();
            Ok(())
        }
        Commands::Server { socket, foreground } => {
            info!(
                "Starting server: socket={socket:?}, foreground={foreground}"
            );
            start_server(socket, foreground);
            Ok(())
        }
    }
}

/// Default action when no command is specified
#[allow(clippy::unnecessary_wraps)]
fn default_action(_config: BmuxConfig) -> Result<()> {
    // Try to attach to the most recent session, or create a new one
    info!("No command specified, trying default action");

    // For now, just create a new session
    create_new_session(None, false);
    Ok(())
}

/// Create a new session
fn create_new_session(session_name: Option<String>, _detach: bool) {
    let session_name =
        session_name.unwrap_or_else(|| format!("session-{}", SessionId::new().0.as_simple()));

    println!("Creating new session: {session_name}");

    // TODO: Implement actual session creation
    // For now, just show what would happen
    println!("Session '{session_name}' would be created here");
    println!("This is where the terminal multiplexer would start!");
}

/// Attach to an existing session
fn attach_to_session(
    _target: Option<String>,
    _independent: bool,
    _follow_client: Option<String>,
) {
    println!("Attaching to session...");

    // TODO: Implement actual session attachment
    println!("Session attachment would happen here");
}

/// List all sessions
fn list_sessions() {
    println!("Active sessions:");

    // TODO: Implement actual session listing
    println!("  (No sessions currently running)");
}

/// Kill a session
fn kill_session(target: &str) {
    println!("Killing session: {target}");

    // TODO: Implement actual session killing
    println!("Session '{target}' would be killed here");
}

/// Show server information
fn show_server_info() {
    println!("bmux server information:");
    println!("  Version: {}", env!("CARGO_PKG_VERSION"));
    println!("  Status: Not implemented yet");

    // TODO: Implement actual server info
}

/// Start the bmux server
fn start_server(_socket: Option<std::path::PathBuf>, _foreground: bool) {
    println!("Starting bmux server...");

    // TODO: Implement actual server startup
    println!("Server would start here");
}
