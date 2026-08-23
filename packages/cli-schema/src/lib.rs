#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

//! CLI argument definitions for bmux.
//!
//! This crate contains the clap derive structs and enums that define bmux's
//! command-line interface. It has no runtime dependencies — only `clap`.
//! The docs site uses this to auto-generate the CLI reference page.

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
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingReplayMode {
    Watch,
    Interactive,
    Verify,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingEventKindArg {
    PaneInputRaw,
    PaneOutputRaw,
    ProtocolReplyRaw,
    PaneImage,
    ServerEvent,
    RequestStart,
    RequestDone,
    RequestError,
    Custom,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingListSortArg {
    Started,
    Name,
    Events,
    Size,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingListOrderArg {
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingListStatusArg {
    All,
    Active,
    Done,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingExportFormat {
    Gif,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingRenderMode {
    Font,
    Bitmap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingPaletteSource {
    Auto,
    Recording,
    Terminal,
    Xterm,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingCursorMode {
    Auto,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingCursorShape {
    Auto,
    Block,
    Bar,
    Underline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingCursorBlinkMode {
    Auto,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingCursorProfile {
    Auto,
    Ghostty,
    Generic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingCursorPaintMode {
    Auto,
    Invert,
    Fill,
    Outline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingCursorTextMode {
    Auto,
    SwapFgBg,
    ForceContrast,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum HostedModeArg {
    P2p,
    ControlPlane,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SandboxEnvModeArg {
    Clean,
    Inherit,
    Hermetic,
}

fn parse_runtime_name(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("runtime name cannot be empty".to_string());
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
    Err("runtime name can only include letters, numbers, '-', '_' or '.'".to_string())
}

#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
#[command(name = "bmux")]
#[command(about = "Server-backed terminal multiplexer CLI")]
pub struct Cli {
    /// Merge an additional config file (highest precedence layer)
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<String>,

    /// Execute command against a configured target (local or remote)
    #[arg(id = "connection_target", long = "target", global = true)]
    pub target: Option<String>,

    /// Select named runtime instance (default: `default`)
    #[arg(long, global = true, value_parser = parse_runtime_name)]
    pub runtime: Option<String>,

    /// Internal: bypass plugin command dispatch and use core handlers only
    #[arg(long, hide = true, global = true)]
    pub core_builtins_only: bool,

    #[command(subcommand)]
    pub command: Option<Command>,

    /// Enable verbose logging
    #[arg(short, long)]
    pub verbose: bool,

    /// Set log level for file logging
    #[arg(long, global = true, value_enum)]
    pub log_level: Option<LogLevel>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// First-run setup wizard for hosted mode (p2p default)
    Setup {
        /// Check hosted readiness without starting or changing runtime state
        #[arg(long)]
        check: bool,
        /// Hosted operation mode (`p2p` is infra-light default)
        #[arg(long, value_enum)]
        mode: Option<HostedModeArg>,
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
        mode: Option<HostedModeArg>,
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
    /// SSH-key access controls for iroh hosting
    Access {
        #[command(subcommand)]
        command: AccessCommand,
    },
    /// SSH kiosk profiles and token management
    Kiosk {
        #[command(subcommand)]
        command: KioskCommand,
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
    /// Kill a session by name or UUID
    KillSession {
        /// Session name or UUID
        target: String,
        /// Bypass policy checks for local kill operations
        #[arg(long)]
        force_local: bool,
    },
    /// Kill all sessions
    KillAllSessions {
        /// Bypass policy checks for local kill operations
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
    /// Session management commands
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Remote target utilities
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Server lifecycle and status tools
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    /// Logging diagnostics and utilities
    Logs {
        #[command(subcommand)]
        command: LogsCommand,
    },
    /// Structured diagnostics event viewer
    Diagnostics {
        #[command(subcommand)]
        command: DiagnosticsCommand,
    },
    /// Configuration management and inspection
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Runtime performance telemetry controls
    Perf {
        #[command(subcommand)]
        command: PerfCommand,
    },
    /// Run system-wide health checks
    Doctor {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
        /// Run hosted-mode focused checks
        #[arg(long)]
        hosted: bool,
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
    /// Headless playbook execution and testing
    Playbook {
        #[command(subcommand)]
        command: PlaybookCommand,
    },
    /// Run bmux in an isolated ephemeral sandbox environment
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommand,
    },
    /// Manage bmux slots (multi-version installs).
    Slot {
        #[command(subcommand)]
        command: SlotCommand,
    },
    /// Alias for `bmux slot ...` (same subcommand tree; useful alongside the
    /// standalone `bmux-env` binary for symmetry).
    Env {
        #[command(subcommand)]
        command: SlotCommand,
    },
    /// Internal sshenv device-seal broker used by panes.
    DeviceSealBroker,
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
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

/// Slot-management commands.
///
/// Both `bmux slot ...` and `bmux env ...` map to this enum; they are
/// aliases for the same implementation.
#[derive(Debug, Subcommand)]
pub enum SlotCommand {
    /// List all declared slots and the presentational default.
    List {
        /// Output format.
        #[arg(long, value_enum, default_value_t = SlotOutputFormat::Toml)]
        format: SlotOutputFormat,
    },
    /// Show one slot's full resolved detail.
    Show {
        /// Slot name. Defaults to the active slot.
        name: Option<String>,
        /// Output format.
        #[arg(long, value_enum, default_value_t = SlotOutputFormat::Toml)]
        format: SlotOutputFormat,
    },
    /// Print this slot's resolved paths (config/runtime/data/state/log).
    Paths {
        /// Slot name. Defaults to the active slot.
        name: Option<String>,
    },
    /// Validate the slot manifest: names, duplicate runtime dirs, binaries,
    /// per-slot configs.
    Doctor,
    /// Register a new slot.
    ///
    /// Writes to `~/.config/bmux/slots.toml` and places `bmux-<name>` into
    /// the bin dir. When the manifest is read-only (e.g. under /nix/store)
    /// prints the block and exits with code 77 without touching disk.
    ///
    /// When a slot with the same name already exists, pass `--overwrite`
    /// to replace it. On an interactive TTY you will be prompted for
    /// confirmation unless `--yes` is also passed.
    Install {
        /// Slot name (validates as `[A-Za-z0-9._-]+`, not reserved).
        name: String,
        /// Path to the source `bmux` binary.
        binary: String,
        /// Disable base-config inheritance for this slot.
        #[arg(long)]
        no_inherit_base: bool,
        /// Symlink (default) or copy the binary into bin_dir.
        #[arg(long, value_enum, default_value_t = SlotInstallMode::Symlink)]
        mode: SlotInstallMode,
        /// Destination bin dir for `bmux-<name>`. Defaults to ~/.local/bin
        /// (or `$BMUX_SLOTS_BIN_DIR`).
        #[arg(long)]
        bin_dir: Option<std::path::PathBuf>,
        /// Output format for the printed block.
        #[arg(long, value_enum, default_value_t = SlotOutputFormat::Toml)]
        format: SlotOutputFormat,
        /// Do not modify disk; only print what would happen.
        #[arg(long)]
        dry_run: bool,
        /// Replace an existing slot with the same name. Without this flag
        /// duplicates are refused (after an interactive prompt, if a TTY
        /// is attached).
        #[arg(long)]
        overwrite: bool,
        /// Skip interactive confirmation prompts. When replacing an
        /// existing slot, `--overwrite` is still required.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Remove a slot from the manifest and delete its `bmux-<name>` binary.
    Uninstall {
        /// Slot name.
        name: String,
        /// Also remove the slot's config/data/state/log dirs.
        #[arg(long)]
        purge: bool,
        /// Destination bin dir the slot binary lives in.
        #[arg(long)]
        bin_dir: Option<std::path::PathBuf>,
    },
    /// Print shell code that prepends `$BMUX_SLOTS_BIN_DIR` to `PATH`.
    Shell {
        #[arg(long, value_enum, default_value_t = SlotShellKind::Auto)]
        shell: SlotShellKind,
    },
    /// Run a command with a slot's env applied (re-execs via execvp).
    Exec {
        /// Slot name to activate.
        slot: String,
        /// Command and arguments.
        #[arg(trailing_var_arg = true, required = true)]
        argv: Vec<String>,
    },
    /// Print the resolved env-var set as structured data.
    Print {
        #[arg(long, value_enum, default_value_t = SlotPrintFormat::Shell)]
        format: SlotPrintFormat,
    },
}

/// Output format for slot subcommands that support structured output.
#[derive(Debug, Copy, Clone, clap::ValueEnum)]
pub enum SlotOutputFormat {
    Toml,
    Json,
    Nix,
}

/// Shell dialect for `bmux slot shell` / `bmux env shell`.
#[derive(Debug, Copy, Clone, clap::ValueEnum)]
pub enum SlotShellKind {
    Auto,
    Bash,
    Zsh,
    Fish,
    Nushell,
    Powershell,
    Posix,
}

/// Output format for `bmux slot print` / `bmux env print`.
#[derive(Debug, Copy, Clone, clap::ValueEnum)]
pub enum SlotPrintFormat {
    Shell,
    Json,
    Nix,
    Fish,
}

/// Placement mode for the per-slot binary during install.
#[derive(Debug, Copy, Clone, clap::ValueEnum)]
pub enum SlotInstallMode {
    /// Symlink into the bin dir (default).
    Symlink,
    /// Copy into the bin dir.
    Copy,
}

#[derive(Debug, Subcommand)]
pub enum AccessCommand {
    /// Show iroh SSH access status
    Status,
    /// Initialize iroh SSH access and enable it immediately
    Init {
        /// Add all currently loaded SSH agent keys
        #[arg(long)]
        agent: bool,
        /// Add a public key from file (repeatable)
        #[arg(long, value_name = "PATH")]
        key_file: Vec<String>,
        /// Add a public key line directly (repeatable)
        #[arg(long, value_name = "KEY")]
        public_key: Vec<String>,
        /// Import public keys from a GitHub username (repeatable)
        #[arg(long, value_name = "USER")]
        github_user: Vec<String>,
        /// Skip interactive confirmation prompts
        #[arg(long)]
        yes: bool,
    },
    /// Add SSH keys to iroh allowlist
    Add {
        /// Add all currently loaded SSH agent keys
        #[arg(long)]
        agent: bool,
        /// Add a public key from file (repeatable)
        #[arg(long, value_name = "PATH")]
        key_file: Vec<String>,
        /// Add a public key line directly (repeatable)
        #[arg(long, value_name = "KEY")]
        public_key: Vec<String>,
        /// Import public keys from a GitHub username (repeatable)
        #[arg(long, value_name = "USER")]
        github_user: Vec<String>,
    },
    /// List currently authorized SSH keys
    List {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove an SSH key from the allowlist
    Remove {
        /// Key fingerprint to remove
        fingerprint: String,
        /// Skip interactive confirmation prompts
        #[arg(long)]
        yes: bool,
    },
    /// Enable iroh SSH access enforcement
    Enable,
    /// Disable iroh SSH access enforcement
    Disable,
}

#[derive(Debug, Subcommand)]
pub enum KioskCommand {
    /// Show kiosk profile status and effective defaults
    Status {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Initialize/update kiosk bootstrap files from config
    Init {
        /// Kiosk profile to reconcile (repeatable)
        #[arg(long, value_name = "PROFILE", conflicts_with = "all_profiles")]
        profile: Vec<String>,
        /// Reconcile all configured profiles
        #[arg(long, conflicts_with = "profile")]
        all_profiles: bool,
        /// Preview actions without writing files
        #[arg(long)]
        dry_run: bool,
        /// Skip confirmation prompts
        #[arg(long)]
        yes: bool,
    },
    /// Issue a kiosk token scoped to a profile
    IssueToken {
        /// Kiosk profile name
        profile: String,
        /// Optional session override stored in token
        #[arg(long)]
        session: Option<String>,
        /// Optional token TTL in seconds (defaults to profile/default TTL)
        #[arg(long)]
        ttl_secs: Option<u64>,
        /// Mark token as one-shot (defaults to profile/default one_shot)
        #[arg(long, conflicts_with = "multi_use")]
        one_shot: bool,
        /// Mark token as reusable until expiry
        #[arg(long, conflicts_with = "one_shot")]
        multi_use: bool,
    },
    /// Revoke one issued kiosk token by id
    RevokeToken {
        /// Token id to revoke
        token_id: String,
    },
    /// Attach using a kiosk token (intended for forced SSH command)
    Attach {
        /// Kiosk profile name
        profile: String,
        /// Raw kiosk token
        #[arg(long)]
        token: String,
    },
    /// Print generated sshd include text without writing files
    SshPrintConfig {
        /// Kiosk profile to print (repeatable)
        #[arg(long, value_name = "PROFILE", conflicts_with = "all_profiles")]
        profile: Vec<String>,
        /// Print all configured profiles
        #[arg(long, conflicts_with = "profile")]
        all_profiles: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum RemoteCommand {
    /// List configured connection targets
    List {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Verify connectivity to a configured target
    Test {
        /// Target name or ssh destination
        target: String,
    },
    /// Diagnose remote connectivity and runtime readiness
    Doctor {
        /// Target name or ssh destination
        target: String,
        /// Apply safe automatic fixes when possible
        #[arg(long)]
        fix: bool,
    },
    /// Create and validate a named remote target profile
    Init {
        /// Name for the target profile
        name: String,
        /// Configure as an SSH target (user@host[:port] or host)
        #[arg(long, conflicts_with_all = ["tls", "iroh"])]
        ssh: Option<String>,
        /// Configure as a TLS target (host[:port])
        #[arg(long, conflicts_with_all = ["ssh", "iroh"])]
        tls: Option<String>,
        /// Configure as an iroh target (endpoint_id[?relay=https://...][&auth=ssh])
        #[arg(long, conflicts_with_all = ["ssh", "tls"])]
        iroh: Option<String>,
        /// SSH username override
        #[arg(long)]
        user: Option<String>,
        /// SSH/TLS port override
        #[arg(long)]
        port: Option<u16>,
        /// Mark as default target
        #[arg(long)]
        set_default: bool,
    },
    /// Ensure remote bmux runtime is installed and reachable
    InstallServer {
        /// Target name
        target: String,
    },
    /// Upgrade remote bmux runtime for one or all targets
    Upgrade {
        /// Target name (omit to upgrade all configured targets)
        target: Option<String>,
    },
    /// Manage TLS gateway trust pins
    Trust {
        #[command(subcommand)]
        command: RemoteTrustCommand,
    },
    /// Shell completion helpers for targets/sessions
    Complete {
        #[command(subcommand)]
        command: RemoteCompleteCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum RemoteTrustCommand {
    /// List effective TLS gateway trust pins
    List,
    /// Probe and trust a TLS gateway
    Add {
        /// TLS target URL or host[:port]
        target: String,
        /// Trust without an interactive confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Remove a local TLS gateway trust pin
    Remove {
        /// Endpoint key (`host:port`)
        endpoint: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum RemoteCompleteCommand {
    /// Print target names for completion
    Targets,
    /// Print session names for a target
    Sessions {
        /// Target name
        target: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum PlaybookCommand {
    /// Run a playbook from a file or stdin
    Run {
        /// Path to playbook file, or `-` for stdin
        source: String,
        /// Output results as JSON
        #[arg(long)]
        json: bool,
        /// Run visual interactive live tour (TTY; non-TTY falls back to prompt controls)
        #[arg(long)]
        interactive: bool,
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
    },
    /// Validate a playbook without executing it
    Validate {
        /// Path to playbook file, or `-` for stdin
        source: String,
        /// Output results as JSON
        #[arg(long)]
        json: bool,
    },
    /// Start an interactive playbook session with a socket for agent control
    Interactive {
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
    },
    /// Dry-run: parse, validate, and print the execution plan without running
    DryRun {
        /// Path to playbook file, or `-` for stdin
        source: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Compare results from two playbook runs
    Diff {
        /// Path to first (baseline/left) playbook result JSON
        left: String,
        /// Path to second (new/right) playbook result JSON
        right: String,
        /// Output diff as JSON
        #[arg(long)]
        json: bool,
        /// Timing regression threshold in percent (default: 50)
        #[arg(long, default_value = "50")]
        timing_threshold: u64,
    },
    /// Clean up sandbox temp directories from previous playbook runs
    Cleanup {
        /// Only list orphaned dirs without deleting
        #[arg(long)]
        dry_run: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum SandboxCommand {
    /// Run bmux in a dev-friendly isolated sandbox (prefers local debug build)
    Dev {
        /// Path to bmux binary to execute (default: ./target/debug/bmux if present)
        #[arg(long)]
        bmux_bin: Option<String>,
        /// Sandbox environment mode
        #[arg(long, value_enum, default_value = "clean")]
        env_mode: SandboxEnvModeArg,
        /// Output sandbox metadata as JSON
        #[arg(long)]
        json: bool,
        /// Print fully resolved environment map before executing command
        #[arg(long)]
        print_env: bool,
        /// Kill sandbox command if it exceeds this timeout in seconds
        #[arg(long)]
        timeout: Option<u64>,
        /// Optional human-friendly sandbox label
        #[arg(long)]
        name: Option<String>,
        /// bmux arguments to execute inside sandbox (pass after --)
        #[arg(
            required = true,
            num_args = 1..,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        command: Vec<String>,
    },
    /// Run bmux in an isolated ephemeral environment
    Run {
        /// Path to bmux binary to execute (default: current executable)
        #[arg(long)]
        bmux_bin: Option<String>,
        /// Sandbox environment mode
        #[arg(long, value_enum, default_value = "clean")]
        env_mode: SandboxEnvModeArg,
        /// Keep sandbox directory after command exits
        #[arg(long)]
        keep: bool,
        /// Output sandbox metadata as JSON
        #[arg(long)]
        json: bool,
        /// Print fully resolved environment map before executing command
        #[arg(long)]
        print_env: bool,
        /// Kill sandbox command if it exceeds this timeout in seconds
        #[arg(long)]
        timeout: Option<u64>,
        /// Optional human-friendly sandbox label
        #[arg(long)]
        name: Option<String>,
        /// bmux arguments to execute inside sandbox (pass after --)
        #[arg(
            required = true,
            num_args = 1..,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        command: Vec<String>,
    },
    /// List known sandbox directories and runtime status
    List {
        /// Filter to matching status only
        #[arg(long, value_enum, default_value = "all")]
        status: SandboxStatusArg,
        /// Filter to a sandbox source
        #[arg(long, value_enum, default_value = "all")]
        source: SandboxSourceArg,
        /// Maximum entries to show
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show a summary of sandbox runtime and index health
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Inspect a sandbox by id or absolute path
    Inspect {
        /// Sandbox id (bmux-sbx-...) or full path
        sandbox: Option<String>,
        /// Inspect the most recent sandbox
        #[arg(long, conflicts_with_all = ["latest_failed", "sandbox"])]
        latest: bool,
        /// Inspect the most recent failed sandbox
        #[arg(long, conflicts_with_all = ["latest", "sandbox"])]
        latest_failed: bool,
        /// Filter source when resolving --latest or --latest-failed
        #[arg(long, value_enum, default_value = "all")]
        source: SandboxSourceArg,
        /// Number of log lines to tail from sandbox logs
        #[arg(long, default_value_t = 80)]
        tail: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Print sandbox log tail without full manifest output
    Tail {
        /// Sandbox id (bmux-sbx-...) or full path
        sandbox: Option<String>,
        /// Tail the most recent sandbox
        #[arg(long, conflicts_with_all = ["latest_failed", "sandbox"])]
        latest: bool,
        /// Tail the most recent failed sandbox
        #[arg(long, conflicts_with_all = ["latest", "sandbox"])]
        latest_failed: bool,
        /// Filter source when resolving --latest or --latest-failed
        #[arg(long, value_enum, default_value = "all")]
        source: SandboxSourceArg,
        /// Number of log lines to tail from sandbox logs
        #[arg(long, default_value_t = 80)]
        tail: usize,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Open sandbox paths and repro context quickly
    Open {
        /// Sandbox id (bmux-sbx-...) or full path
        sandbox: Option<String>,
        /// Open the most recent sandbox
        #[arg(long, conflicts_with_all = ["latest_failed", "sandbox"])]
        latest: bool,
        /// Open the most recent failed sandbox
        #[arg(long, conflicts_with_all = ["latest", "sandbox"])]
        latest_failed: bool,
        /// Filter source when resolving --latest or --latest-failed
        #[arg(long, value_enum, default_value = "all")]
        source: SandboxSourceArg,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Rerun command from an existing sandbox manifest
    Rerun {
        /// Sandbox id (bmux-sbx-...) or full path
        sandbox: Option<String>,
        /// Rerun the most recent sandbox
        #[arg(long, conflicts_with_all = ["latest_failed", "sandbox"])]
        latest: bool,
        /// Rerun the most recent failed sandbox
        #[arg(long, conflicts_with_all = ["latest", "sandbox"])]
        latest_failed: bool,
        /// Filter source when resolving --latest or --latest-failed
        #[arg(long, value_enum, default_value = "all")]
        source: SandboxSourceArg,
        /// Override bmux binary path from manifest
        #[arg(long)]
        bmux_bin: Option<String>,
        /// Override sandbox environment mode from manifest
        #[arg(long, value_enum)]
        env_mode: Option<SandboxEnvModeArg>,
        /// Keep rerun sandbox directory after command exits
        #[arg(long)]
        keep: bool,
        /// Output sandbox metadata as JSON
        #[arg(long)]
        json: bool,
        /// Print fully resolved environment map before executing command
        #[arg(long)]
        print_env: bool,
        /// Kill rerun command if it exceeds this timeout in seconds
        #[arg(long)]
        timeout: Option<u64>,
        /// Optional human-friendly sandbox label
        #[arg(long)]
        name: Option<String>,
    },
    /// One-shot sandbox failure triage summary
    Triage {
        /// Sandbox id (bmux-sbx-...) or full path
        sandbox: Option<String>,
        /// Triage the most recent sandbox
        #[arg(long, conflicts_with_all = ["latest_failed", "sandbox"])]
        latest: bool,
        /// Triage the most recent failed sandbox
        #[arg(long, conflicts_with_all = ["latest", "sandbox"])]
        latest_failed: bool,
        /// Filter source when resolving --latest or --latest-failed
        #[arg(long, value_enum, default_value = "all")]
        source: SandboxSourceArg,
        /// Number of log lines to tail from sandbox logs
        #[arg(long, default_value_t = 80)]
        tail: usize,
        /// Rerun command from selected sandbox manifest
        #[arg(long)]
        rerun: bool,
        /// Override bmux binary path from manifest for rerun
        #[arg(long)]
        bmux_bin: Option<String>,
        /// Override sandbox environment mode from manifest for rerun
        #[arg(long, value_enum)]
        env_mode: Option<SandboxEnvModeArg>,
        /// Keep rerun sandbox directory after command exits
        #[arg(long)]
        keep: bool,
        /// Print fully resolved environment map before rerun command
        #[arg(long)]
        print_env: bool,
        /// Kill rerun command if it exceeds this timeout in seconds
        #[arg(long)]
        timeout: Option<u64>,
        /// Optional human-friendly sandbox label for rerun
        #[arg(long)]
        name: Option<String>,
        /// Bundle selected sandbox diagnostics after triage
        #[arg(long)]
        bundle: bool,
        /// Optional output directory for triage bundle (default: ./sandbox-bundles)
        #[arg(long, requires = "bundle")]
        bundle_output: Option<String>,
        /// Fail triage when bundle verification reports unexpected extra artifacts
        #[arg(long, requires = "bundle")]
        bundle_strict_verify: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Diagnose sandbox readiness and health checks
    Doctor {
        /// Optional sandbox id/path for targeted checks
        #[arg(long)]
        id: Option<String>,
        /// Apply automatic sandbox repair actions
        #[arg(long)]
        fix: bool,
        /// Preview repair actions without mutating state (requires --fix)
        #[arg(long, requires = "fix")]
        dry_run: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Bundle sandbox diagnostics and logs into a single directory
    Bundle {
        /// Sandbox id (bmux-sbx-...) or full path
        sandbox: String,
        /// Optional output directory path (default: ./sandbox-bundles)
        #[arg(long)]
        output: Option<String>,
        /// Include environment/context snapshot in bundle
        #[arg(long)]
        include_env: bool,
        /// Include current sandbox index state in bundle
        #[arg(long)]
        include_index_state: bool,
        /// Include doctor checks snapshot in bundle
        #[arg(long)]
        include_doctor: bool,
        /// Verify generated bundle against recorded metadata
        #[arg(long)]
        verify: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Verify a sandbox bundle against its recorded metadata
    VerifyBundle {
        /// Bundle directory path
        bundle_dir: String,
        /// Fail when bundle contains unexpected extra artifacts
        #[arg(long)]
        strict: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Clean up sandbox temp directories from sandbox runs
    Cleanup {
        /// Only list orphaned dirs without deleting
        #[arg(long)]
        dry_run: bool,
        /// Remove only failed/aborted sandboxes
        #[arg(long, conflicts_with = "all_status")]
        failed_only: bool,
        /// Include both failed and non-failed sandboxes
        #[arg(long, conflicts_with = "failed_only")]
        all_status: bool,
        /// Minimum age in seconds before sandbox is eligible for cleanup
        #[arg(long)]
        older_than: Option<u64>,
        /// Filter to a sandbox source
        #[arg(long, value_enum)]
        source: Option<SandboxSourceArg>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Opinionated cleanup defaults for day-to-day sandbox hygiene
    Clean {
        /// Only list orphaned dirs without deleting
        #[arg(long)]
        dry_run: bool,
        /// Include both failed and non-failed sandboxes
        #[arg(long)]
        all_status: bool,
        /// Minimum age in seconds before sandbox is eligible for cleanup
        #[arg(long)]
        older_than: Option<u64>,
        /// Filter to a sandbox source
        #[arg(long, value_enum)]
        source: Option<SandboxSourceArg>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Rebuild sandbox index from discovered sandbox manifests
    RebuildIndex {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SandboxStatusArg {
    Running,
    Stopped,
    Failed,
    All,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SandboxSourceArg {
    SandboxCli,
    Playbook,
    RecordingVerify,
    All,
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
    /// Kill a session by name or UUID
    Kill {
        /// Session name or UUID
        target: String,
        /// Bypass policy checks for local kill operations
        #[arg(long)]
        force_local: bool,
    },
    /// Kill all sessions
    KillAll {
        /// Bypass policy checks for local kill operations
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
pub enum ServerCommand {
    /// Start local bmux server
    Start {
        /// Run server in background daemon mode
        #[arg(long)]
        daemon: bool,
        /// Internal flag used by daemon launcher
        #[arg(long, hide = true)]
        foreground_internal: bool,
        /// Force-enable pane shell integration hooks for this server start
        #[arg(long, conflicts_with = "no_pane_shell_integration")]
        pane_shell_integration: bool,
        /// Disable pane shell integration hooks for this server start
        #[arg(long, conflicts_with = "pane_shell_integration")]
        no_pane_shell_integration: bool,
        /// Enable hidden rolling recording on server boot for this run
        #[arg(long, conflicts_with = "no_rolling_recording")]
        rolling_recording: bool,
        /// Disable hidden rolling recording on server boot for this run
        #[arg(
            long,
            conflicts_with = "rolling_recording",
            conflicts_with_all = [
                "rolling_window_secs",
                "rolling_event_kind_all",
                "rolling_event_kind",
                "rolling_capture_input",
                "no_rolling_capture_input",
                "rolling_capture_output",
                "no_rolling_capture_output",
                "rolling_capture_events",
                "no_rolling_capture_events",
                "rolling_capture_protocol_replies",
                "no_rolling_capture_protocol_replies",
                "rolling_capture_images",
                "no_rolling_capture_images"
            ]
        )]
        no_rolling_recording: bool,
        /// Override rolling recording window in seconds for this run
        #[arg(long, value_name = "SECONDS", conflicts_with = "no_rolling_recording")]
        rolling_window_secs: Option<u64>,
        /// Enable all supported rolling event kinds
        #[arg(long, conflicts_with = "rolling_event_kind")]
        rolling_event_kind_all: bool,
        /// Explicit rolling event kind allowlist (repeatable)
        #[arg(long, value_enum, conflicts_with = "rolling_event_kind_all")]
        rolling_event_kind: Vec<RecordingEventKindArg>,
        /// Override rolling capture of pane input bytes for this run
        #[arg(long, conflicts_with = "no_rolling_capture_input")]
        rolling_capture_input: bool,
        /// Disable rolling capture of pane input bytes for this run
        #[arg(long, conflicts_with = "rolling_capture_input")]
        no_rolling_capture_input: bool,
        /// Override rolling capture of pane output bytes for this run
        #[arg(long, conflicts_with = "no_rolling_capture_output")]
        rolling_capture_output: bool,
        /// Disable rolling capture of pane output bytes for this run
        #[arg(long, conflicts_with = "rolling_capture_output")]
        no_rolling_capture_output: bool,
        /// Override rolling capture of lifecycle/request/custom events for this run
        #[arg(long, conflicts_with = "no_rolling_capture_events")]
        rolling_capture_events: bool,
        /// Disable rolling capture of lifecycle/request/custom events for this run
        #[arg(long, conflicts_with = "rolling_capture_events")]
        no_rolling_capture_events: bool,
        /// Override rolling capture of protocol reply bytes for this run
        #[arg(long, conflicts_with = "no_rolling_capture_protocol_replies")]
        rolling_capture_protocol_replies: bool,
        /// Disable rolling capture of protocol reply bytes for this run
        #[arg(long, conflicts_with = "rolling_capture_protocol_replies")]
        no_rolling_capture_protocol_replies: bool,
        /// Override rolling capture of extracted pane images for this run
        #[arg(long, conflicts_with = "no_rolling_capture_images")]
        rolling_capture_images: bool,
        /// Disable rolling capture of extracted pane images for this run
        #[arg(long, conflicts_with = "rolling_capture_images")]
        no_rolling_capture_images: bool,
    },
    /// Check server status
    Status {
        /// Print server status as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show caller and server control principal identities
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
    /// Manage native per-user server login autostart
    Autostart {
        #[command(subcommand)]
        command: ServerAutostartCommand,
    },
    /// Run a TLS gateway that exposes bmux over TCP/TLS
    Gateway {
        /// Listen address (host:port)
        #[arg(long)]
        listen: String,
        /// Expose gateway publicly via reverse SSH tunnel helper
        #[arg(long)]
        host: bool,
        /// Hosting mode used by --host (iroh is default)
        #[arg(long, value_enum, default_value_t = GatewayHostMode::Iroh)]
        host_mode: GatewayHostMode,
        /// Reverse SSH relay destination (user@host)
        #[arg(long, default_value = "nokey@localhost.run")]
        host_relay: String,
        /// Generate and use self-signed cert/key in runtime dir for quick setup
        #[arg(long)]
        quick: bool,
        /// PEM encoded certificate chain path
        #[arg(long, requires = "key_file")]
        cert_file: Option<String>,
        /// PEM encoded private key path (PKCS8)
        #[arg(long, requires = "cert_file")]
        key_file: Option<String>,
    },
    /// Internal stdio bridge used by SSH transport
    #[command(hide = true)]
    Bridge {
        /// Bridge framed IPC over stdin/stdout
        #[arg(long)]
        stdio: bool,
        /// Validate bridge stdio cleanliness and readiness
        #[arg(long, hide = true)]
        preflight: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ServerAutostartCommand {
    /// Install or update native per-user login autostart
    Install {
        /// Enable autostart without starting the service immediately
        #[arg(long)]
        no_start: bool,
        /// Executable path to persist in the native service declaration
        #[arg(long, value_name = "PATH")]
        executable: Option<String>,
    },
    /// Remove bmux-managed native per-user login autostart
    Uninstall,
    /// Show native autostart declaration, manager, and server status
    Status {
        /// Print status as JSON
        #[arg(long)]
        json: bool,
    },
    /// Print the native service declaration without installing it
    Print {
        /// Executable path to render in the native service declaration
        #[arg(long, value_name = "PATH")]
        executable: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
pub enum GatewayHostMode {
    Iroh,
    Ssh,
}

#[derive(Debug, Subcommand)]
pub enum KeymapCommand {
    /// Print compiled keymap and overlap diagnostics
    Doctor {
        /// Print diagnostics as JSON
        #[arg(long)]
        json: bool,
    },
    /// Explain effective action for a key chord
    Explain {
        /// Key chord to resolve (e.g. "ctrl+b n", "alt+h", "escape")
        key: String,
        /// Resolve inside a specific modal mode id
        #[arg(long)]
        mode: Option<String>,
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum DiagnosticsCommand {
    /// Interactive structured diagnostics event viewer with dynamic filters
    Watch {
        /// Number of recent events to preload (defaults to saved profile value or 200)
        #[arg(long)]
        lines: Option<usize>,
        /// Show entries newer than a relative duration (e.g. 30s, 10m, 2h, 1d)
        #[arg(long)]
        since: Option<String>,
        /// State profile for saved watch filters (default: global `default`)
        #[arg(long)]
        profile: Option<String>,
        /// Include regex filter (case-sensitive, repeatable)
        #[arg(long = "include")]
        include: Vec<String>,
        /// Include regex filter (case-insensitive, repeatable)
        #[arg(long = "include-i")]
        include_i: Vec<String>,
        /// Exclude regex filter (case-sensitive, repeatable)
        #[arg(long = "exclude")]
        exclude: Vec<String>,
        /// Exclude regex filter (case-insensitive, repeatable)
        #[arg(long = "exclude-i")]
        exclude_i: Vec<String>,
    },
    /// Alias for watch
    View {
        /// Number of recent events to preload (defaults to saved profile value or 200)
        #[arg(long)]
        lines: Option<usize>,
        /// Show entries newer than a relative duration (e.g. 30s, 10m, 2h, 1d)
        #[arg(long)]
        since: Option<String>,
        /// State profile for saved watch filters (default: global `default`)
        #[arg(long)]
        profile: Option<String>,
        /// Include regex filter (case-sensitive, repeatable)
        #[arg(long = "include")]
        include: Vec<String>,
        /// Include regex filter (case-insensitive, repeatable)
        #[arg(long = "include-i")]
        include_i: Vec<String>,
        /// Exclude regex filter (case-sensitive, repeatable)
        #[arg(long = "exclude")]
        exclude: Vec<String>,
        /// Exclude regex filter (case-insensitive, repeatable)
        #[arg(long = "exclude-i")]
        exclude_i: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum LogsCommand {
    /// Print effective log file path
    Path {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Print effective runtime log level
    Level {
        /// Print output as JSON
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
    Watch {
        /// Number of recent lines to preload (defaults to saved profile value or 200)
        #[arg(long)]
        lines: Option<usize>,
        /// Show entries newer than a relative duration (e.g. 30s, 10m, 2h, 1d)
        #[arg(long)]
        since: Option<String>,
        /// State profile for saved watch filters (default: global `default`)
        #[arg(long)]
        profile: Option<String>,
        /// Include regex filter (case-sensitive, repeatable)
        #[arg(long = "include")]
        include: Vec<String>,
        /// Include regex filter (case-insensitive, repeatable)
        #[arg(long = "include-i")]
        include_i: Vec<String>,
        /// Exclude regex filter (case-sensitive, repeatable)
        #[arg(long = "exclude")]
        exclude: Vec<String>,
        /// Exclude regex filter (case-insensitive, repeatable)
        #[arg(long = "exclude-i")]
        exclude_i: Vec<String>,
    },
    /// Manage saved log watch profiles
    Profiles {
        #[command(subcommand)]
        command: LogsProfilesCommand,
    },
}

#[derive(Debug, Subcommand)]
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

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the config file path
    Path {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Print the effective configuration
    Show {
        /// Print output as JSON instead of TOML
        #[arg(long)]
        json: bool,
    },
    /// Get a configuration value by dotted key path
    Get {
        /// Dotted key path (e.g. plugins.settings.bmux.tab_strip.height, behavior.mouse.enabled)
        key: String,
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Set a configuration value in the config file
    Set {
        /// Dotted key path (e.g. plugins.settings.bmux.tab_strip.height, behavior.mouse.enabled)
        key: String,
        /// Value to set (booleans, integers, and strings are auto-detected)
        value: String,
    },
    /// Manage composition profiles
    Profiles {
        #[command(subcommand)]
        command: ConfigProfilesCommand,
    },
    /// Inspect scoped configuration resolution
    Scope {
        #[command(subcommand)]
        command: ConfigScopeCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigScopeCommand {
    /// Explain scoped source discovery and effective config
    Explain {
        /// Scope name to resolve, e.g. pane, new-pane, or global
        #[arg(long, default_value = "pane")]
        scope: String,
        /// Cwd used for discovering local bmux.toml overlays
        #[arg(long)]
        cwd: Option<std::path::PathBuf>,
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigProfilesCommand {
    /// List known profile ids
    List {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show one profile patch and inheritance metadata
    Show {
        /// Profile id
        profile: String,
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show resolved active profile and effective layers
    Resolve {
        /// Optional forced profile id
        profile: Option<String>,
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Explain layer-by-layer composition changes
    Explain {
        /// Optional forced profile id
        profile: Option<String>,
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Set active profile in config file
    Switch {
        /// Profile id to activate
        profile: String,
        /// Preview changes without writing config
        #[arg(long)]
        dry_run: bool,
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Diff two resolved profile configurations
    Diff {
        /// From profile id
        from: String,
        /// To profile id
        to: String,
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Validate profile graph and layer rules
    Lint {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Evaluate auto-select rules in current environment
    Evaluate {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
pub enum PerfProfileArg {
    Basic,
    Detailed,
    Trace,
}

#[derive(Debug, Subcommand)]
pub enum PerfCommand {
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
        AccessCommand, AuthCommand, Cli, Command, ConfigCommand, ConfigProfilesCommand,
        GatewayHostMode, HostedModeArg, KeymapCommand, KioskCommand, LogsCommand,
        LogsProfilesCommand, PerfCommand, PerfProfileArg, PlaybookCommand, RecordingEventKindArg,
        RemoteCommand, RemoteCompleteCommand, SandboxCommand, SandboxEnvModeArg, SandboxSourceArg,
        SandboxStatusArg, ServerAutostartCommand, ServerCommand, SessionCommand, TerminalCommand,
        TraceFamily,
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
    fn parses_global_config_flag() {
        let cli = Cli::try_parse_from(["bmux", "--config", "./custom.toml", "keymap", "doctor"])
            .expect("valid CLI args");
        assert_eq!(cli.config.as_deref(), Some("./custom.toml"));
    }

    #[test]
    fn parses_keymap_explain_subcommand() {
        let cli = Cli::try_parse_from([
            "bmux", "keymap", "explain", "ctrl+b n", "--mode", "normal", "--json",
        ])
        .expect("valid CLI args");
        let Some(Command::Keymap { command }) = cli.command else {
            panic!("expected keymap subcommand");
        };
        assert!(matches!(
            command,
            KeymapCommand::Explain { key, mode, json }
                if key == "ctrl+b n" && mode.as_deref() == Some("normal") && json
        ));
    }

    #[test]
    fn parses_config_profiles_explain_subcommand() {
        let cli = Cli::try_parse_from([
            "bmux",
            "config",
            "profiles",
            "explain",
            "zellij_compat",
            "--json",
        ])
        .expect("valid CLI args");
        let Some(Command::Config {
            command: ConfigCommand::Profiles { command },
        }) = cli.command
        else {
            panic!("expected config profiles command");
        };
        assert!(matches!(
            command,
            ConfigProfilesCommand::Explain { profile, json }
                if profile.as_deref() == Some("zellij_compat") && json
        ));
    }

    #[test]
    fn parses_config_profiles_switch_dry_run_subcommand() {
        let cli = Cli::try_parse_from([
            "bmux",
            "config",
            "profiles",
            "switch",
            "tmux_compat",
            "--dry-run",
            "--json",
        ])
        .expect("valid CLI args");
        let Some(Command::Config {
            command: ConfigCommand::Profiles { command },
        }) = cli.command
        else {
            panic!("expected config profiles command");
        };
        assert!(matches!(
            command,
            ConfigProfilesCommand::Switch {
                profile,
                dry_run,
                json,
            } if profile == "tmux_compat" && dry_run && json
        ));
    }

    #[test]
    fn parses_connect_command_with_session() {
        let cli = Cli::try_parse_from(["bmux", "connect", "prod", "app"]).expect("valid CLI args");
        let Some(Command::Connect {
            target,
            session,
            follow,
            global,
            reconnect_forever,
        }) = cli.command
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
    fn parses_remote_test_command() {
        let cli = Cli::try_parse_from(["bmux", "remote", "test", "prod"]).expect("valid CLI args");
        let Some(Command::Remote { command }) = cli.command else {
            panic!("expected remote command");
        };
        assert!(matches!(
            command,
            RemoteCommand::Test { target } if target == "prod"
        ));
    }

    #[test]
    fn parses_remote_doctor_fix_flag() {
        let cli = Cli::try_parse_from(["bmux", "remote", "doctor", "prod", "--fix"])
            .expect("valid CLI args");
        let Some(Command::Remote { command }) = cli.command else {
            panic!("expected remote command");
        };
        assert!(matches!(
            command,
            RemoteCommand::Doctor { target, fix } if target == "prod" && fix
        ));
    }

    #[test]
    fn parses_remote_complete_sessions_command() {
        let cli = Cli::try_parse_from(["bmux", "remote", "complete", "sessions", "prod"])
            .expect("valid CLI args");
        let Some(Command::Remote { command }) = cli.command else {
            panic!("expected remote command");
        };
        assert!(matches!(
            command,
            RemoteCommand::Complete {
                command: RemoteCompleteCommand::Sessions { target }
            } if target == "prod"
        ));
    }

    #[test]
    fn parses_setup_command_defaults() {
        let cli = Cli::try_parse_from(["bmux", "setup"]).expect("valid CLI args");
        let Some(Command::Setup { check, mode }) = cli.command else {
            panic!("expected setup command");
        };
        assert!(!check);
        assert!(mode.is_none());
    }

    #[test]
    fn parses_setup_check_flag() {
        let cli = Cli::try_parse_from(["bmux", "setup", "--check"]).expect("valid CLI args");
        let Some(Command::Setup { check, mode }) = cli.command else {
            panic!("expected setup command");
        };
        assert!(check);
        assert!(mode.is_none());
    }

    #[test]
    fn parses_setup_mode_flag() {
        let cli = Cli::try_parse_from(["bmux", "setup", "--mode", "control-plane"])
            .expect("valid CLI args");
        let Some(Command::Setup { mode, .. }) = cli.command else {
            panic!("expected setup command");
        };
        assert_eq!(mode, Some(HostedModeArg::ControlPlane));
    }

    #[test]
    fn parses_hosts_defaults() {
        let cli = Cli::try_parse_from(["bmux", "hosts"]).expect("valid CLI args");
        let Some(Command::Hosts { verbose }) = cli.command else {
            panic!("expected hosts command");
        };
        assert!(!verbose);
    }

    #[test]
    fn parses_hosts_verbose_flag() {
        let cli = Cli::try_parse_from(["bmux", "hosts", "--verbose"]).expect("valid CLI args");
        let Some(Command::Hosts { verbose }) = cli.command else {
            panic!("expected hosts command");
        };
        assert!(verbose);
    }

    #[test]
    fn parses_host_command_defaults() {
        let cli = Cli::try_parse_from(["bmux", "host"]).expect("valid CLI args");
        let Some(Command::Host {
            listen,
            name,
            copy,
            daemon,
            status,
            stop,
            restart,
            mode,
        }) = cli.command
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
        let cli = Cli::try_parse_from(["bmux", "host", "--copy"]).expect("valid CLI args");
        let Some(Command::Host { copy, .. }) = cli.command else {
            panic!("expected host command");
        };
        assert!(copy);
    }

    #[test]
    fn parses_host_status_flag() {
        let cli = Cli::try_parse_from(["bmux", "host", "--status"]).expect("valid CLI args");
        let Some(Command::Host { status, .. }) = cli.command else {
            panic!("expected host command");
        };
        assert!(status);
    }

    #[test]
    fn parses_host_stop_flag() {
        let cli = Cli::try_parse_from(["bmux", "host", "--stop"]).expect("valid CLI args");
        let Some(Command::Host { stop, .. }) = cli.command else {
            panic!("expected host command");
        };
        assert!(stop);
    }

    #[test]
    fn parses_host_daemon_flag() {
        let cli = Cli::try_parse_from(["bmux", "host", "--daemon"]).expect("valid CLI args");
        let Some(Command::Host { daemon, .. }) = cli.command else {
            panic!("expected host command");
        };
        assert!(daemon);
    }

    #[test]
    fn parses_host_restart_flag() {
        let cli = Cli::try_parse_from(["bmux", "host", "--restart"]).expect("valid CLI args");
        let Some(Command::Host { restart, .. }) = cli.command else {
            panic!("expected host command");
        };
        assert!(restart);
    }

    #[test]
    fn parses_host_mode_flag() {
        let cli = Cli::try_parse_from(["bmux", "host", "--mode", "p2p"]).expect("valid CLI args");
        let Some(Command::Host { mode, .. }) = cli.command else {
            panic!("expected host command");
        };
        assert_eq!(mode, Some(HostedModeArg::P2p));
    }

    #[test]
    fn parses_auth_login_no_browser_flag() {
        let cli =
            Cli::try_parse_from(["bmux", "auth", "login", "--no-browser"]).expect("valid CLI args");
        let Some(Command::Auth { command }) = cli.command else {
            panic!("expected auth command");
        };
        assert!(matches!(command, AuthCommand::Login { no_browser: true }));
    }

    #[test]
    fn parses_auth_status_command() {
        let cli = Cli::try_parse_from(["bmux", "auth", "status"]).expect("valid CLI args");
        let Some(Command::Auth { command }) = cli.command else {
            panic!("expected auth command");
        };
        assert!(matches!(command, AuthCommand::Status));
    }

    #[test]
    fn parses_auth_logout_command() {
        let cli = Cli::try_parse_from(["bmux", "auth", "logout"]).expect("valid CLI args");
        let Some(Command::Auth { command }) = cli.command else {
            panic!("expected auth command");
        };
        assert!(matches!(command, AuthCommand::Logout));
    }

    #[test]
    fn parses_access_status_command() {
        let cli = Cli::try_parse_from(["bmux", "access", "status"]).expect("valid CLI args");
        let Some(Command::Access { command }) = cli.command else {
            panic!("expected access command");
        };
        assert!(matches!(command, AccessCommand::Status));
    }

    #[test]
    fn parses_access_init_agent_yes_flags() {
        let cli = Cli::try_parse_from(["bmux", "access", "init", "--agent", "--yes"])
            .expect("valid CLI args");
        let Some(Command::Access { command }) = cli.command else {
            panic!("expected access command");
        };
        assert!(matches!(
            command,
            AccessCommand::Init {
                agent: true,
                yes: true,
                ..
            }
        ));
    }

    #[test]
    fn parses_access_add_key_file() {
        let cli = Cli::try_parse_from([
            "bmux",
            "access",
            "add",
            "--key-file",
            "~/.ssh/id_ed25519.pub",
        ])
        .expect("valid CLI args");
        let Some(Command::Access { command }) = cli.command else {
            panic!("expected access command");
        };
        let AccessCommand::Add { key_file, .. } = command else {
            panic!("expected access add command");
        };
        assert_eq!(key_file, vec!["~/.ssh/id_ed25519.pub".to_string()]);
    }

    #[test]
    fn parses_access_remove_with_yes_flag() {
        let cli = Cli::try_parse_from(["bmux", "access", "remove", "abc123", "--yes"])
            .expect("valid CLI args");
        let Some(Command::Access { command }) = cli.command else {
            panic!("expected access command");
        };
        assert!(matches!(
            command,
            AccessCommand::Remove {
                fingerprint,
                yes: true
            } if fingerprint == "abc123"
        ));
    }

    #[test]
    fn parses_kiosk_status_json_command() {
        let cli =
            Cli::try_parse_from(["bmux", "kiosk", "status", "--json"]).expect("valid CLI args");
        let Some(Command::Kiosk { command }) = cli.command else {
            panic!("expected kiosk command");
        };
        assert!(matches!(command, KioskCommand::Status { json: true }));
    }

    #[test]
    fn parses_kiosk_issue_token_flags() {
        let cli = Cli::try_parse_from([
            "bmux",
            "kiosk",
            "issue-token",
            "demo",
            "--session",
            "main",
            "--ttl-secs",
            "900",
            "--multi-use",
        ])
        .expect("valid CLI args");
        let Some(Command::Kiosk { command }) = cli.command else {
            panic!("expected kiosk command");
        };
        assert!(matches!(
            command,
            KioskCommand::IssueToken {
                profile,
                session,
                ttl_secs,
                one_shot: false,
                multi_use: true,
            } if profile == "demo" && session.as_deref() == Some("main") && ttl_secs == Some(900)
        ));
    }

    #[test]
    fn parses_share_flags() {
        let cli = Cli::try_parse_from([
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
        }) = cli.command
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
        let cli = Cli::try_parse_from(["bmux", "share", "revoke", "demo"]).expect("valid CLI args");
        let Some(Command::Share {
            target, secondary, ..
        }) = cli.command
        else {
            panic!("expected share command");
        };
        assert_eq!(target.as_deref(), Some("revoke"));
        assert_eq!(secondary.as_deref(), Some("demo"));
    }

    #[test]
    fn parses_share_qr_flag() {
        let cli = Cli::try_parse_from(["bmux", "share", "--qr"]).expect("valid CLI args");
        let Some(Command::Share { qr, .. }) = cli.command else {
            panic!("expected share command");
        };
        assert!(qr);
    }

    #[test]
    fn parses_doctor_hosted_flag() {
        let cli = Cli::try_parse_from(["bmux", "doctor", "--hosted"]).expect("valid CLI args");
        let Some(Command::Doctor { hosted, json }) = cli.command else {
            panic!("expected doctor command");
        };
        assert!(hosted);
        assert!(!json);
    }

    #[test]
    fn parses_join_command_with_link() {
        let cli =
            Cli::try_parse_from(["bmux", "join", "bmux://demo", "main"]).expect("valid CLI args");
        let Some(Command::Join { link, session }) = cli.command else {
            panic!("expected join command");
        };
        assert_eq!(link.as_deref(), Some("bmux://demo"));
        assert_eq!(session.as_deref(), Some("main"));
    }

    #[test]
    fn parses_global_target_flag() {
        let cli = Cli::try_parse_from(["bmux", "--target", "prod", "list-sessions"])
            .expect("valid CLI args");
        assert_eq!(cli.target.as_deref(), Some("prod"));
        assert!(matches!(
            cli.command,
            Some(Command::ListSessions { json: false })
        ));
    }

    #[test]
    fn parses_global_runtime_flag() {
        let cli = Cli::try_parse_from(["bmux", "--runtime", "dev", "server", "status"])
            .expect("valid CLI args");
        assert_eq!(cli.runtime.as_deref(), Some("dev"));
    }

    #[test]
    fn rejects_invalid_runtime_flag() {
        let error = Cli::try_parse_from(["bmux", "--runtime", "dev/runtime", "server", "status"])
            .expect_err("invalid runtime should fail");
        let text = error.to_string();
        assert!(text.contains("invalid value") || text.contains("runtime name"));
    }

    #[test]
    fn parses_perf_status_json_flag() {
        let cli =
            Cli::try_parse_from(["bmux", "perf", "status", "--json"]).expect("valid CLI args");
        let Some(Command::Perf { command }) = cli.command else {
            panic!("expected perf subcommand");
        };
        assert!(matches!(command, PerfCommand::Status { json: true }));
    }

    #[test]
    fn parses_perf_on_defaults_to_detailed_profile() {
        let cli = Cli::try_parse_from(["bmux", "perf", "on"]).expect("valid CLI args");
        let Some(Command::Perf { command }) = cli.command else {
            panic!("expected perf subcommand");
        };
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
        let cli = Cli::try_parse_from(["bmux", "perf", "on", "--profile", "trace", "--json"])
            .expect("valid CLI args");
        let Some(Command::Perf { command }) = cli.command else {
            panic!("expected perf subcommand");
        };
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
        let cli = Cli::try_parse_from(["bmux", "perf", "off", "--json"]).expect("valid CLI args");
        let Some(Command::Perf { command }) = cli.command else {
            panic!("expected perf subcommand");
        };
        assert!(matches!(command, PerfCommand::Off { json: true }));
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
                foreground_internal: false,
                pane_shell_integration: false,
                no_pane_shell_integration: false,
                rolling_recording: false,
                no_rolling_recording: false,
                rolling_window_secs: None,
                rolling_event_kind_all: false,
                rolling_event_kind,
                rolling_capture_input: false,
                no_rolling_capture_input: false,
                rolling_capture_output: false,
                no_rolling_capture_output: false,
                rolling_capture_events: false,
                no_rolling_capture_events: false,
                rolling_capture_protocol_replies: false,
                no_rolling_capture_protocol_replies: false,
                rolling_capture_images: false,
                no_rolling_capture_images: false,
            } if rolling_event_kind.is_empty()
        ));
    }

    #[test]
    fn parses_server_start_with_rolling_kind_and_category_overrides() {
        let cli = Cli::try_parse_from([
            "bmux",
            "server",
            "start",
            "--rolling-event-kind",
            "protocol-reply-raw",
            "--rolling-event-kind",
            "pane-image",
            "--rolling-capture-input",
            "--no-rolling-capture-events",
        ])
        .expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(
            command,
            ServerCommand::Start {
                rolling_event_kind,
                rolling_capture_input: true,
                no_rolling_capture_events: true,
                ..
            } if rolling_event_kind == vec![
                RecordingEventKindArg::ProtocolReplyRaw,
                RecordingEventKindArg::PaneImage,
            ]
        ));
    }

    #[test]
    fn rejects_conflicting_server_start_rolling_capture_flags() {
        let error = Cli::try_parse_from([
            "bmux",
            "server",
            "start",
            "--rolling-capture-output",
            "--no-rolling-capture-output",
        ])
        .expect_err("conflicting rolling capture flags should fail");
        assert!(error.to_string().contains("cannot be used"));
    }

    #[test]
    fn rejects_conflicting_server_start_rolling_kind_flags() {
        let error = Cli::try_parse_from([
            "bmux",
            "server",
            "start",
            "--rolling-event-kind-all",
            "--rolling-event-kind",
            "pane-output-raw",
        ])
        .expect_err("conflicting rolling kind flags should fail");
        assert!(error.to_string().contains("cannot be used"));
    }

    #[test]
    fn parses_server_start_with_rolling_event_kind_all_flag() {
        let cli = Cli::try_parse_from(["bmux", "server", "start", "--rolling-event-kind-all"])
            .expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(
            command,
            ServerCommand::Start {
                rolling_event_kind_all: true,
                rolling_event_kind,
                ..
            } if rolling_event_kind.is_empty()
        ));
    }

    #[test]
    fn parses_server_start_with_rolling_window_and_kinds() {
        let cli = Cli::try_parse_from([
            "bmux",
            "server",
            "start",
            "--rolling-window-secs",
            "180",
            "--rolling-event-kind",
            "pane-output-raw",
        ])
        .expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(
            command,
            ServerCommand::Start {
                rolling_window_secs: Some(180),
                rolling_event_kind,
                ..
            } if rolling_event_kind == vec![RecordingEventKindArg::PaneOutputRaw]
        ));
    }

    #[test]
    fn rejects_server_start_no_rolling_with_window_override() {
        let error = Cli::try_parse_from([
            "bmux",
            "server",
            "start",
            "--no-rolling-recording",
            "--rolling-window-secs",
            "90",
        ])
        .expect_err("conflicting flags should fail");
        assert!(error.to_string().contains("cannot be used"));
    }

    #[test]
    fn rejects_server_start_no_rolling_with_capture_override() {
        let error = Cli::try_parse_from([
            "bmux",
            "server",
            "start",
            "--no-rolling-recording",
            "--rolling-capture-output",
        ])
        .expect_err("conflicting flags should fail");
        assert!(error.to_string().contains("cannot be used"));
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
                foreground_internal: false,
                rolling_recording: false,
                no_rolling_recording: false,
                rolling_window_secs: None,
                ..
            }
        ));
    }

    #[test]
    fn parses_server_start_with_rolling_recording_flag() {
        let cli = Cli::try_parse_from(["bmux", "server", "start", "--rolling-recording"])
            .expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(
            command,
            ServerCommand::Start {
                rolling_recording: true,
                no_rolling_recording: false,
                ..
            }
        ));
    }

    #[test]
    fn parses_server_start_with_no_rolling_recording_flag() {
        let cli = Cli::try_parse_from(["bmux", "server", "start", "--no-rolling-recording"])
            .expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(
            command,
            ServerCommand::Start {
                rolling_recording: false,
                no_rolling_recording: true,
                ..
            }
        ));
    }

    #[test]
    fn parses_server_start_with_no_pane_shell_integration_flag() {
        let cli = Cli::try_parse_from(["bmux", "server", "start", "--no-pane-shell-integration"])
            .expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(
            command,
            ServerCommand::Start {
                pane_shell_integration: false,
                no_pane_shell_integration: true,
                ..
            }
        ));
    }

    #[test]
    fn rejects_conflicting_server_start_pane_shell_integration_flags() {
        let error = Cli::try_parse_from([
            "bmux",
            "server",
            "start",
            "--pane-shell-integration",
            "--no-pane-shell-integration",
        ])
        .expect_err("conflicting flags should fail");
        assert!(error.to_string().contains("cannot be used"));
    }

    #[test]
    fn parses_server_start_with_rolling_window_override() {
        let cli = Cli::try_parse_from(["bmux", "server", "start", "--rolling-window-secs", "180"])
            .expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(
            command,
            ServerCommand::Start {
                rolling_window_secs: Some(180),
                ..
            }
        ));
    }

    #[test]
    fn rejects_conflicting_server_start_rolling_flags() {
        let error = Cli::try_parse_from([
            "bmux",
            "server",
            "start",
            "--rolling-recording",
            "--no-rolling-recording",
        ])
        .expect_err("conflicting flags should fail");
        assert!(error.to_string().contains("cannot be used"));
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
    fn parses_server_autostart_commands() {
        let cli = Cli::try_parse_from([
            "bmux",
            "server",
            "autostart",
            "install",
            "--no-start",
            "--executable",
            "/opt/bmux bin/bmux",
        ])
        .expect("valid autostart install args");
        let Some(Command::Server {
            command: ServerCommand::Autostart { command },
        }) = cli.command
        else {
            panic!("expected server autostart subcommand");
        };
        assert!(matches!(
            command,
            ServerAutostartCommand::Install {
                no_start: true,
                executable: Some(path),
            } if path == "/opt/bmux bin/bmux"
        ));

        let cli = Cli::try_parse_from(["bmux", "server", "autostart", "status", "--json"])
            .expect("valid autostart status args");
        assert!(matches!(
            cli.command,
            Some(Command::Server {
                command: ServerCommand::Autostart {
                    command: ServerAutostartCommand::Status { json: true },
                },
            })
        ));

        let cli = Cli::try_parse_from(["bmux", "server", "autostart", "uninstall"])
            .expect("valid autostart uninstall args");
        assert!(matches!(
            cli.command,
            Some(Command::Server {
                command: ServerCommand::Autostart {
                    command: ServerAutostartCommand::Uninstall,
                },
            })
        ));

        let cli = Cli::try_parse_from(["bmux", "server", "autostart", "print"])
            .expect("valid autostart print args");
        assert!(matches!(
            cli.command,
            Some(Command::Server {
                command: ServerCommand::Autostart {
                    command: ServerAutostartCommand::Print { executable: None },
                },
            })
        ));
    }

    #[test]
    fn parses_server_gateway_command() {
        let cli = Cli::try_parse_from([
            "bmux",
            "server",
            "gateway",
            "--listen",
            "0.0.0.0:7443",
            "--cert-file",
            "cert.pem",
            "--key-file",
            "key.pem",
        ])
        .expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(
            command,
            ServerCommand::Gateway {
                listen,
                host,
                host_mode,
                host_relay,
                quick,
                cert_file,
                key_file,
            } if listen == "0.0.0.0:7443"
                && !host
                && host_mode == GatewayHostMode::Iroh
                && host_relay == "nokey@localhost.run"
                && !quick
                && cert_file.as_deref() == Some("cert.pem")
                && key_file.as_deref() == Some("key.pem")
        ));
    }

    #[test]
    fn parses_server_gateway_quick_mode() {
        let cli = Cli::try_parse_from([
            "bmux",
            "server",
            "gateway",
            "--listen",
            "0.0.0.0:7443",
            "--quick",
        ])
        .expect("valid CLI args");
        let Some(Command::Server { command }) = cli.command else {
            panic!("expected server subcommand");
        };
        assert!(matches!(
            command,
            ServerCommand::Gateway {
                listen,
                host,
                host_mode,
                host_relay,
                quick,
                cert_file,
                key_file,
            } if listen == "0.0.0.0:7443"
                && !host
                && host_mode == GatewayHostMode::Iroh
                && host_relay == "nokey@localhost.run"
                && quick
                && cert_file.is_none()
                && key_file.is_none()
        ));
    }

    #[test]
    fn parses_logs_path_subcommand() {
        let cli = Cli::try_parse_from(["bmux", "logs", "path"]).expect("valid CLI args");
        let Some(Command::Logs { command }) = cli.command else {
            panic!("expected logs subcommand");
        };
        assert!(matches!(command, LogsCommand::Path { json: false }));
    }

    #[test]
    fn parses_logs_level_subcommand() {
        let cli = Cli::try_parse_from(["bmux", "logs", "level"]).expect("valid CLI args");
        let Some(Command::Logs { command }) = cli.command else {
            panic!("expected logs subcommand");
        };
        assert!(matches!(command, LogsCommand::Level { json: false }));
    }

    #[test]
    fn parses_logs_tail_flags() {
        let cli = Cli::try_parse_from(["bmux", "logs", "tail", "--lines", "10", "--no-follow"])
            .expect("valid CLI args");
        let Some(Command::Logs { command }) = cli.command else {
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
        let Some(Command::Logs { command }) = cli.command else {
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
        let Some(Command::Logs { command }) = cli.command else {
            panic!("expected logs subcommand");
        };
        assert!(matches!(
            command,
            LogsCommand::Watch {
                lines: Some(150),
                since: Some(ref value),
                profile: None,
                include,
                include_i,
                exclude,
                exclude_i,
            } if value == "2h"
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
        let Some(Command::Logs { command }) = cli.command else {
            panic!("expected logs subcommand");
        };
        assert!(matches!(
            command,
            LogsCommand::Watch {
                lines: None,
                since: None,
                profile: Some(ref value),
                include,
                include_i,
                exclude,
                exclude_i,
            } if value == "incident-db"
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
        let Some(Command::Logs { command }) = cli.command else {
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
        let Some(Command::Logs { command }) = cli.command else {
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
            Some(Command::Logs {
                command:
                    LogsCommand::Profiles {
                        command: LogsProfilesCommand::Delete { profile }
                    }
            }) if profile == "incident-db"
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
            Some(Command::Logs {
                command:
                    LogsCommand::Profiles {
                        command: LogsProfilesCommand::Rename { from, to }
                    }
            }) if from == "incident-db" && to == "incident-db-2"
        ));
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
        assert_eq!(cli.target, None);
        assert_eq!(follow, None);
        assert!(!global);
    }

    #[test]
    fn parses_attach_target_separately_from_connection_target() {
        let cli = Cli::try_parse_from(["bmux", "--target", "prod", "attach", "local://dev"])
            .expect("valid CLI args");
        assert_eq!(cli.target.as_deref(), Some("prod"));
        let Some(Command::Attach { target, .. }) = cli.command else {
            panic!("expected attach command");
        };
        assert_eq!(target.as_deref(), Some("local://dev"));
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
    fn parses_top_level_external_hyphenated_command() {
        let cli = Cli::try_parse_from(["bmux", "tool-open", "--name", "editor", "--scope", "dev"])
            .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::External(args))
                if args == vec!["tool-open", "--name", "editor", "--scope", "dev"]
        ));
    }

    #[test]
    fn parses_top_level_external_json_flag() {
        let cli = Cli::try_parse_from(["bmux", "tool-list", "--json"]).expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::External(args)) if args == vec!["tool-list", "--json"]
        ));
    }

    #[test]
    fn parses_top_level_external_positional_and_option_args() {
        let cli = Cli::try_parse_from(["bmux", "tool-close", "active", "--scope", "dev"])
            .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::External(args))
                if args == vec!["tool-close", "active", "--scope", "dev"]
        ));
    }

    #[test]
    fn parses_top_level_external_multiword_flag_command() {
        let cli = Cli::try_parse_from(["bmux", "tool-close-all", "--scope", "dev"])
            .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::External(args)) if args == vec!["tool-close-all", "--scope", "dev"]
        ));
    }

    #[test]
    fn parses_top_level_external_single_token_command() {
        let cli = Cli::try_parse_from(["bmux", "tool-close-all"]).expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::External(args)) if args == vec!["tool-close-all"]
        ));
    }

    #[test]
    fn parses_top_level_external_boolean_flag() {
        let cli = Cli::try_parse_from(["bmux", "tool-close-all", "--force-local"])
            .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::External(args)) if args == vec!["tool-close-all", "--force-local"]
        ));
    }

    #[test]
    fn parses_top_level_external_target_selector_command() {
        let cli = Cli::try_parse_from(["bmux", "tool-focus", "editor"]).expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::External(args)) if args == vec!["tool-focus", "editor"]
        ));
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
    fn parses_top_level_external_policy_command() {
        let cli = Cli::try_parse_from(["bmux", "roles", "--scope", "dev"]).expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::External(args)) if args == vec!["roles", "--scope", "dev"]
        ));
    }

    #[test]
    fn parses_top_level_external_policy_watch_command() {
        let cli = Cli::try_parse_from(["bmux", "roles", "--scope", "dev", "--watch"])
            .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::External(args))
                if args == vec!["roles", "--scope", "dev", "--watch"]
        ));
    }

    #[test]
    fn parses_top_level_external_assign_command() {
        let cli = Cli::try_parse_from([
            "bmux",
            "assign",
            "--scope",
            "dev",
            "--subject",
            "550e8400-e29b-41d4-a716-446655440000",
            "--level",
            "writer",
        ])
        .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::External(args))
                if args == vec![
                    "assign",
                    "--scope",
                    "dev",
                    "--subject",
                    "550e8400-e29b-41d4-a716-446655440000",
                    "--level",
                    "writer",
                ]
        ));
    }

    #[test]
    fn parses_top_level_external_unassign_command() {
        let cli = Cli::try_parse_from([
            "bmux",
            "unassign",
            "--scope",
            "dev",
            "--subject",
            "550e8400-e29b-41d4-a716-446655440000",
        ])
        .expect("valid CLI args");
        assert!(matches!(
            cli.command,
            Some(Command::External(args))
                if args == vec![
                    "unassign",
                    "--scope",
                    "dev",
                    "--subject",
                    "550e8400-e29b-41d4-a716-446655440000",
                ]
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
    fn static_session_namespace_rejects_plugin_owned_subcommands() {
        let error = Cli::try_parse_from(["bmux", "session", "roles", "--scope", "dev"])
            .expect_err("static CLI should reject plugin-owned session descendant");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
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

    #[test]
    fn parses_external_plugin_command_path() {
        let cli = Cli::try_parse_from(["bmux", "vendor", "roles", "dev"]).expect("valid CLI args");
        let Some(Command::External(args)) = cli.command else {
            panic!("expected external plugin command path");
        };
        assert_eq!(args, vec!["vendor", "roles", "dev"]);
    }

    #[test]
    fn parses_playbook_run_interactive_flag() {
        let cli = Cli::try_parse_from([
            "bmux",
            "playbook",
            "run",
            "fixtures/echo.dsl",
            "--interactive",
        ])
        .expect("valid CLI args");
        let Some(Command::Playbook { command }) = cli.command else {
            panic!("expected playbook command");
        };
        assert!(matches!(
            command,
            PlaybookCommand::Run {
                interactive: true,
                ..
            }
        ));
    }

    #[test]
    fn parses_sandbox_run_defaults() {
        let cli = Cli::try_parse_from(["bmux", "sandbox", "run", "--", "server", "status"])
            .expect("valid CLI args");
        let Some(Command::Sandbox { command }) = cli.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::Run {
                bmux_bin: None,
                env_mode: SandboxEnvModeArg::Clean,
                keep: false,
                json: false,
                print_env: false,
                timeout: None,
                name: None,
                command,
            } if command == vec!["server".to_string(), "status".to_string()]
        ));
    }

    #[test]
    fn parses_sandbox_dev_defaults() {
        let cli = Cli::try_parse_from(["bmux", "sandbox", "dev", "--", "attach"])
            .expect("valid CLI args");
        let Some(Command::Sandbox { command }) = cli.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::Dev {
                bmux_bin: None,
                env_mode: SandboxEnvModeArg::Clean,
                json: false,
                print_env: false,
                timeout: None,
                name: None,
                command,
            } if command == vec!["attach".to_string()]
        ));
    }

    #[test]
    fn parses_sandbox_run_overrides() {
        let cli = Cli::try_parse_from([
            "bmux",
            "sandbox",
            "run",
            "--bmux-bin",
            "./target/debug/bmux",
            "--env-mode",
            "inherit",
            "--keep",
            "--json",
            "--print-env",
            "--timeout",
            "30",
            "--name",
            "my-check",
            "--",
            "attach",
        ])
        .expect("valid CLI args");
        let Some(Command::Sandbox { command }) = cli.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::Run {
                bmux_bin: Some(ref bin),
                env_mode: SandboxEnvModeArg::Inherit,
                keep: true,
                json: true,
                print_env: true,
                timeout: Some(30),
                name: Some(ref name),
                command,
            } if bin == "./target/debug/bmux"
                && name == "my-check"
                && command == vec!["attach".to_string()]
        ));
    }

    #[test]
    fn parses_sandbox_list_and_inspect() {
        let list = Cli::try_parse_from([
            "bmux", "sandbox", "list", "--status", "failed", "--source", "playbook", "--limit",
            "5", "--json",
        ])
        .expect("valid list args");
        let Some(Command::Sandbox { command }) = list.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::List {
                status: SandboxStatusArg::Failed,
                source: SandboxSourceArg::Playbook,
                limit: 5,
                json: true,
            }
        ));

        let status = Cli::try_parse_from(["bmux", "sandbox", "status", "--json"])
            .expect("valid status args");
        let Some(Command::Sandbox { command }) = status.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(command, SandboxCommand::Status { json: true }));

        let inspect =
            Cli::try_parse_from(["bmux", "sandbox", "inspect", "bmux-sbx-abc", "--tail", "25"])
                .expect("valid inspect args");
        let Some(Command::Sandbox { command }) = inspect.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::Inspect {
                sandbox: Some(target),
                latest: false,
                latest_failed: false,
                source: SandboxSourceArg::All,
                tail: 25,
                json: false,
            } if target == "bmux-sbx-abc"
        ));

        let latest = Cli::try_parse_from(["bmux", "sandbox", "inspect", "--latest", "--json"])
            .expect("valid latest inspect args");
        let Some(Command::Sandbox { command }) = latest.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::Inspect {
                sandbox: None,
                latest: true,
                latest_failed: false,
                source: SandboxSourceArg::All,
                tail: 80,
                json: true,
            }
        ));

        let latest_failed = Cli::try_parse_from([
            "bmux",
            "sandbox",
            "inspect",
            "--latest-failed",
            "--tail",
            "40",
        ])
        .expect("valid latest failed inspect args");
        let Some(Command::Sandbox { command }) = latest_failed.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::Inspect {
                sandbox: None,
                latest: false,
                latest_failed: true,
                source: SandboxSourceArg::All,
                tail: 40,
                json: false,
            }
        ));

        let latest_by_source = Cli::try_parse_from([
            "bmux", "sandbox", "inspect", "--latest", "--source", "playbook", "--json",
        ])
        .expect("valid latest inspect source args");
        let Some(Command::Sandbox { command }) = latest_by_source.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::Inspect {
                sandbox: None,
                latest: true,
                latest_failed: false,
                source: SandboxSourceArg::Playbook,
                tail: 80,
                json: true,
            }
        ));

        let tail = Cli::try_parse_from([
            "bmux",
            "sandbox",
            "tail",
            "--latest-failed",
            "--source",
            "recording-verify",
            "--tail",
            "50",
            "--json",
        ])
        .expect("valid tail args");
        let Some(Command::Sandbox { command }) = tail.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::Tail {
                sandbox: None,
                latest: false,
                latest_failed: true,
                source: SandboxSourceArg::RecordingVerify,
                tail: 50,
                json: true,
            }
        ));

        let open = Cli::try_parse_from(["bmux", "sandbox", "open", "--latest", "--json"])
            .expect("valid open args");
        let Some(Command::Sandbox { command }) = open.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::Open {
                sandbox: None,
                latest: true,
                latest_failed: false,
                source: SandboxSourceArg::All,
                json: true,
            }
        ));

        let rerun = Cli::try_parse_from([
            "bmux",
            "sandbox",
            "rerun",
            "--latest-failed",
            "--source",
            "playbook",
            "--bmux-bin",
            "./target/debug/bmux",
            "--env-mode",
            "inherit",
            "--keep",
            "--print-env",
            "--timeout",
            "20",
            "--name",
            "rerun-check",
            "--json",
        ])
        .expect("valid rerun args");
        let Some(Command::Sandbox { command }) = rerun.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::Rerun {
                sandbox: None,
                latest: false,
                latest_failed: true,
                source: SandboxSourceArg::Playbook,
                bmux_bin: Some(ref bin),
                env_mode: Some(SandboxEnvModeArg::Inherit),
                keep: true,
                json: true,
                print_env: true,
                timeout: Some(20),
                name: Some(ref name),
            } if bin == "./target/debug/bmux" && name == "rerun-check"
        ));

        let triage = Cli::try_parse_from([
            "bmux",
            "sandbox",
            "triage",
            "--source",
            "playbook",
            "--tail",
            "30",
            "--rerun",
            "--bmux-bin",
            "./target/debug/bmux",
            "--env-mode",
            "clean",
            "--keep",
            "--print-env",
            "--timeout",
            "15",
            "--name",
            "triage-rerun",
            "--bundle",
            "--bundle-output",
            "./artifacts",
            "--bundle-strict-verify",
            "--json",
        ])
        .expect("valid triage args");
        let Some(Command::Sandbox { command }) = triage.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::Triage {
                sandbox: None,
                latest: false,
                latest_failed: false,
                source: SandboxSourceArg::Playbook,
                tail: 30,
                rerun: true,
                bmux_bin: Some(ref bin),
                env_mode: Some(SandboxEnvModeArg::Clean),
                keep: true,
                print_env: true,
                timeout: Some(15),
                name: Some(ref name),
                bundle: true,
                bundle_output: Some(ref output),
                bundle_strict_verify: true,
                json: true,
            } if bin == "./target/debug/bmux"
                && name == "triage-rerun"
                && output == "./artifacts"
        ));
    }

    #[test]
    fn parses_sandbox_doctor() {
        let cli = Cli::try_parse_from([
            "bmux",
            "sandbox",
            "doctor",
            "--id",
            "bmux-sbx-123",
            "--fix",
            "--dry-run",
            "--json",
        ])
        .expect("valid doctor args");
        let Some(Command::Sandbox { command }) = cli.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::Doctor {
                id: Some(id),
                fix: true,
                dry_run: true,
                json: true,
            } if id == "bmux-sbx-123"
        ));
    }

    #[test]
    fn parses_sandbox_bundle() {
        let cli = Cli::try_parse_from([
            "bmux",
            "sandbox",
            "bundle",
            "bmux-sbx-123",
            "--output",
            "./artifacts",
            "--include-env",
            "--include-index-state",
            "--include-doctor",
            "--verify",
            "--json",
        ])
        .expect("valid bundle args");
        let Some(Command::Sandbox { command }) = cli.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::Bundle {
                sandbox: target,
                output: Some(output),
                include_env: true,
                include_index_state: true,
                include_doctor: true,
                verify: true,
                json: true,
            } if target == "bmux-sbx-123" && output == "./artifacts"
        ));
    }

    #[test]
    fn parses_sandbox_cleanup_flags() {
        let cli = Cli::try_parse_from([
            "bmux",
            "sandbox",
            "cleanup",
            "--dry-run",
            "--failed-only",
            "--older-than",
            "600",
            "--source",
            "recording-verify",
            "--json",
        ])
        .expect("valid CLI args");
        let Some(Command::Sandbox { command }) = cli.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::Cleanup {
                dry_run: true,
                failed_only: true,
                all_status: false,
                older_than: Some(600),
                source: Some(SandboxSourceArg::RecordingVerify),
                json: true,
            }
        ));
    }

    #[test]
    fn parses_sandbox_verify_bundle() {
        let cli = Cli::try_parse_from([
            "bmux",
            "sandbox",
            "verify-bundle",
            "./sandbox-bundles/bmux-sbx-123-456",
            "--strict",
            "--json",
        ])
        .expect("valid verify-bundle args");
        let Some(Command::Sandbox { command }) = cli.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::VerifyBundle {
                bundle_dir,
                strict: true,
                json: true
            }
                if bundle_dir == "./sandbox-bundles/bmux-sbx-123-456"
        ));
    }

    #[test]
    fn parses_sandbox_rebuild_index() {
        let cli = Cli::try_parse_from(["bmux", "sandbox", "rebuild-index", "--json"])
            .expect("valid CLI args");
        let Some(Command::Sandbox { command }) = cli.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::RebuildIndex { json: true }
        ));
    }

    #[test]
    fn parses_sandbox_clean_flags() {
        let cli = Cli::try_parse_from([
            "bmux",
            "sandbox",
            "clean",
            "--dry-run",
            "--all-status",
            "--older-than",
            "300",
            "--source",
            "playbook",
            "--json",
        ])
        .expect("valid CLI args");
        let Some(Command::Sandbox { command }) = cli.command else {
            panic!("expected sandbox command");
        };
        assert!(matches!(
            command,
            SandboxCommand::Clean {
                dry_run: true,
                all_status: true,
                older_than: Some(300),
                source: Some(SandboxSourceArg::Playbook),
                json: true,
            }
        ));
    }
}
