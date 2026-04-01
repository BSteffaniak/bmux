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
    Verify,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingProfileArg {
    Full,
    Functional,
    Visual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordingEventKindArg {
    PaneInputRaw,
    PaneOutputRaw,
    ProtocolReplyRaw,
    ServerEvent,
    RequestStart,
    RequestDone,
    RequestError,
    Custom,
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

fn parse_cell_size(value: &str) -> Result<(u16, u16), String> {
    let trimmed = value.trim();
    let (width_raw, height_raw) = trimmed
        .split_once('x')
        .or_else(|| trimmed.split_once('X'))
        .ok_or_else(|| "expected CELL_SIZE format WIDTHxHEIGHT, e.g. 8x16".to_string())?;
    let width = width_raw
        .trim()
        .parse::<u16>()
        .map_err(|_| "cell width must be a positive integer".to_string())?;
    let height = height_raw
        .trim()
        .parse::<u16>()
        .map_err(|_| "cell height must be a positive integer".to_string())?;
    if width == 0 || height == 0 {
        return Err("cell width and height must be greater than zero".to_string());
    }
    Ok((width, height))
}

#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
#[command(name = "bmux")]
#[command(about = "Server-backed terminal multiplexer CLI")]
pub struct Cli {
    /// Start interactive bmux with full-session recording
    #[arg(long)]
    pub record: bool,

    /// Do not capture pane input bytes when using --record
    #[arg(long)]
    pub no_capture_input: bool,

    /// Write recording id to a file when using --record
    #[arg(long)]
    pub recording_id_file: Option<String>,

    /// Recording profile when using --record
    #[arg(long, value_enum)]
    pub record_profile: Option<RecordingProfileArg>,

    /// Explicit event kind allowlist when using --record (repeatable)
    #[arg(long, value_enum)]
    pub record_event_kind: Vec<RecordingEventKindArg>,

    /// Stop the server when exiting a --record run
    #[arg(long)]
    pub stop_server_on_exit: bool,

    /// Execute command against a configured target (local or remote)
    #[arg(long, global = true)]
    pub target: Option<String>,

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
    /// Connect to a target and attach to a session
    Connect {
        /// Target name or ssh destination (user@host[:port] or ssh://...)
        target: String,
        /// Session name or UUID; if omitted in TTY mode a picker is shown
        session: Option<String>,
        /// Follow target client UUID and attach to its selected session
        #[arg(long, conflicts_with = "session")]
        follow: Option<String>,
        /// Keep following across target session switches (requires --follow)
        #[arg(long, requires = "follow")]
        global: bool,
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
    /// Recording and replay controls
    Recording {
        #[command(subcommand)]
        command: RecordingCommand,
    },
    /// Headless playbook execution and testing
    Playbook {
        #[command(subcommand)]
        command: PlaybookCommand,
    },
    #[command(external_subcommand)]
    External(Vec<String>),
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
    },
}

#[derive(Debug, Subcommand)]
pub enum RecordingCommand {
    /// Start explicit full-fidelity recording
    Start {
        /// Restrict capture to one session id
        #[arg(long)]
        session_id: Option<String>,
        /// Do not capture pane input bytes
        #[arg(long)]
        no_capture_input: bool,
        /// Recording profile to use
        #[arg(long, value_enum)]
        profile: Option<RecordingProfileArg>,
        /// Explicit event kind allowlist (repeatable)
        #[arg(long, value_enum)]
        event_kind: Vec<RecordingEventKindArg>,
    },
    /// Stop active recording or one by id
    Stop {
        /// Recording id to stop (defaults to active)
        recording_id: Option<String>,
    },
    /// Show active recording status
    Status {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// List available recordings
    List {
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Delete one recording by id or unique id prefix
    Delete {
        /// Recording UUID or unique UUID prefix
        recording_id: String,
    },
    /// Delete all recordings
    DeleteAll {
        /// Proceed without interactive confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Inspect recording timeline events
    Inspect {
        /// Recording id
        recording_id: String,
        /// Limit number of events
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Filter events by kind
        #[arg(long)]
        kind: Option<String>,
        /// Print output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Replay a recording timeline
    Replay {
        /// Recording id
        recording_id: String,
        /// Replay mode
        #[arg(long, value_enum, default_value_t = RecordingReplayMode::Watch)]
        mode: RecordingReplayMode,
        /// Playback speed multiplier in watch mode
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
        /// Optional target bmux binary for verify mode
        #[arg(long)]
        target_bmux: Option<String>,
        /// Compare with another recording id in verify mode
        #[arg(long)]
        compare_recording: Option<String>,
        /// Optional comma-separated ignore rules in verify mode
        #[arg(long)]
        ignore: Option<String>,
        /// Preserve full recorded input timing in verify mode
        #[arg(long)]
        strict_timing: bool,
        /// Maximum verify runtime in seconds before aborting
        #[arg(long)]
        max_verify_duration: Option<u64>,
        /// Timeout in seconds for target verify server readiness
        #[arg(long)]
        verify_start_timeout: Option<u64>,
    },
    /// Run machine-readable verify smoke report
    VerifySmoke {
        /// Recording id
        recording_id: String,
        /// Optional target bmux binary for verify
        #[arg(long)]
        target_bmux: Option<String>,
        /// Compare with another recording id
        #[arg(long)]
        compare_recording: Option<String>,
        /// Optional comma-separated ignore rules
        #[arg(long)]
        ignore: Option<String>,
        /// Preserve full recorded input timing
        #[arg(long)]
        strict_timing: bool,
        /// Maximum verify runtime in seconds before aborting
        #[arg(long)]
        max_verify_duration: Option<u64>,
        /// Timeout in seconds for target verify server readiness
        #[arg(long)]
        verify_start_timeout: Option<u64>,
    },
    /// Export a recording as media
    Export {
        /// Recording id
        recording_id: String,
        /// Export format
        #[arg(long, value_enum, default_value_t = RecordingExportFormat::Gif)]
        format: RecordingExportFormat,
        /// Output file path
        #[arg(long)]
        output: String,
        /// Override view client id for display track selection
        #[arg(long)]
        view_client: Option<String>,
        /// Playback speed multiplier
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
        /// Target frames per second
        #[arg(long, default_value_t = 12)]
        fps: u32,
        /// Maximum export duration in seconds
        #[arg(long)]
        max_duration: Option<u64>,
        /// Maximum exported frames
        #[arg(long)]
        max_frames: Option<u32>,
        /// Renderer mode for frame rasterization
        #[arg(long, value_enum, default_value_t = RecordingRenderMode::Font)]
        renderer: RecordingRenderMode,
        /// Cell size in pixels as `WIDTHxHEIGHT` (e.g. 8x16)
        #[arg(long, value_parser = parse_cell_size)]
        cell_size: Option<(u16, u16)>,
        /// Override glyph cell width in pixels
        #[arg(long)]
        cell_width: Option<u16>,
        /// Override glyph cell height in pixels
        #[arg(long)]
        cell_height: Option<u16>,
        /// Comma-separated preferred font family names
        #[arg(long)]
        font_family: Option<String>,
        /// Font size in pixels for font renderer
        #[arg(long)]
        font_size: Option<f32>,
        /// Line-height multiplier for font renderer
        #[arg(long)]
        line_height: Option<f32>,
        /// Additional font file path (repeatable)
        #[arg(long)]
        font_path: Vec<String>,
        /// Cursor rendering mode for export
        #[arg(long, value_enum)]
        cursor: Option<RecordingCursorMode>,
        /// Cursor shape override for export
        #[arg(long, value_enum)]
        cursor_shape: Option<RecordingCursorShape>,
        /// Cursor blink mode for export
        #[arg(long, value_enum)]
        cursor_blink: Option<RecordingCursorBlinkMode>,
        /// Cursor blink period in milliseconds
        #[arg(long)]
        cursor_blink_period_ms: Option<u32>,
        /// Cursor color override for export (`auto` or #RRGGBB)
        #[arg(long)]
        cursor_color: Option<String>,
        /// Cursor behavior profile for export timing
        #[arg(long, value_enum)]
        cursor_profile: Option<RecordingCursorProfile>,
        /// Keep cursor solid after activity for this duration in ms
        #[arg(long)]
        cursor_solid_after_activity_ms: Option<u32>,
        /// Keep cursor solid after input activity for this duration in ms
        #[arg(long)]
        cursor_solid_after_input_ms: Option<u32>,
        /// Keep cursor solid after output activity for this duration in ms
        #[arg(long)]
        cursor_solid_after_output_ms: Option<u32>,
        /// Keep cursor solid after cursor movement activity for this duration in ms
        #[arg(long)]
        cursor_solid_after_cursor_ms: Option<u32>,
        /// Cursor paint mode for block cursor rendering
        #[arg(long, value_enum)]
        cursor_paint_mode: Option<RecordingCursorPaintMode>,
        /// Cursor text mode for filled block cursor glyph readability
        #[arg(long, value_enum)]
        cursor_text_mode: Option<RecordingCursorTextMode>,
        /// Cursor bar width as a percent of cell width (1-100)
        #[arg(long)]
        cursor_bar_width_pct: Option<u8>,
        /// Cursor underline height as a percent of cell height (1-100)
        #[arg(long)]
        cursor_underline_height_pct: Option<u8>,
        /// Write export metadata JSON to this path
        #[arg(long)]
        export_metadata: Option<String>,
        /// Disable export progress output
        #[arg(long)]
        no_progress: bool,
    },
    /// Delete completed recordings older than the retention period
    Prune {
        /// Override retention period in days (default: use config value)
        #[arg(long)]
        older_than: Option<u64>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
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
    /// Generate a playbook stub from an existing recording
    FromRecording {
        /// Recording ID (or prefix)
        recording_id: String,
        /// Output file path (default: stdout)
        #[arg(long, short)]
        output: Option<String>,
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
    /// Remove orphaned sandbox temp directories from previous playbook runs
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
    /// Internal stdio bridge used by SSH transport
    #[command(hide = true)]
    Bridge {
        /// Bridge framed IPC over stdin/stdout
        #[arg(long)]
        stdio: bool,
    },
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
        Cli, Command, KeymapCommand, LogsCommand, LogsProfilesCommand, RecordingCommand,
        RecordingCursorBlinkMode, RecordingCursorMode, RecordingCursorPaintMode,
        RecordingCursorProfile, RecordingCursorShape, RecordingCursorTextMode,
        RecordingEventKindArg, RecordingExportFormat, RecordingProfileArg, RecordingRenderMode,
        RecordingReplayMode, RemoteCommand, ServerCommand, SessionCommand, TerminalCommand,
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
    fn parses_connect_command_with_session() {
        let cli = Cli::try_parse_from(["bmux", "connect", "prod", "app"]).expect("valid CLI args");
        let Some(Command::Connect {
            target,
            session,
            follow,
            global,
        }) = cli.command
        else {
            panic!("expected connect command");
        };
        assert_eq!(target, "prod");
        assert_eq!(session.as_deref(), Some("app"));
        assert!(follow.is_none());
        assert!(!global);
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
    fn parses_recording_start_with_defaults() {
        let cli = Cli::try_parse_from(["bmux", "recording", "start"]).expect("valid CLI args");
        let Some(Command::Recording { command }) = cli.command else {
            panic!("expected recording command");
        };
        assert!(matches!(
            command,
            RecordingCommand::Start {
                session_id: None,
                no_capture_input: false,
                profile: None,
                event_kind,
            }
            if event_kind.is_empty()
        ));
    }

    #[test]
    fn parses_recording_start_with_no_capture_input() {
        let cli = Cli::try_parse_from([
            "bmux",
            "recording",
            "start",
            "--session-id",
            "550e8400-e29b-41d4-a716-446655440000",
            "--no-capture-input",
        ])
        .expect("valid CLI args");
        let Some(Command::Recording { command }) = cli.command else {
            panic!("expected recording command");
        };
        assert!(matches!(
            command,
            RecordingCommand::Start {
                session_id: Some(ref id),
                no_capture_input: true,
                profile: None,
                event_kind,
            } if id == "550e8400-e29b-41d4-a716-446655440000"
                && event_kind.is_empty()
        ));
    }

    #[test]
    fn parses_recording_replay_verify_mode() {
        let cli = Cli::try_parse_from([
            "bmux",
            "recording",
            "replay",
            "550e8400-e29b-41d4-a716-446655440000",
            "--mode",
            "verify",
        ])
        .expect("valid CLI args");
        let Some(Command::Recording { command }) = cli.command else {
            panic!("expected recording command");
        };
        assert!(matches!(
            command,
            RecordingCommand::Replay {
                mode: RecordingReplayMode::Verify,
                ..
            }
        ));
    }

    #[test]
    fn parses_recording_delete_with_prefix() {
        let cli = Cli::try_parse_from(["bmux", "recording", "delete", "550e8400"])
            .expect("valid CLI args");
        let Some(Command::Recording { command }) = cli.command else {
            panic!("expected recording command");
        };
        assert!(matches!(
            command,
            RecordingCommand::Delete {
                recording_id: ref id
            } if id == "550e8400"
        ));
    }

    #[test]
    fn parses_recording_delete_all_yes_flag() {
        let cli = Cli::try_parse_from(["bmux", "recording", "delete-all", "--yes"])
            .expect("valid CLI args");
        let Some(Command::Recording { command }) = cli.command else {
            panic!("expected recording command");
        };
        assert!(matches!(command, RecordingCommand::DeleteAll { yes: true }));
    }

    #[test]
    fn parses_recording_replay_verify_tuning_flags() {
        let cli = Cli::try_parse_from([
            "bmux",
            "recording",
            "replay",
            "550e8400-e29b-41d4-a716-446655440000",
            "--mode",
            "verify",
            "--strict-timing",
            "--max-verify-duration",
            "45",
            "--verify-start-timeout",
            "20",
        ])
        .expect("valid CLI args");
        let Some(Command::Recording { command }) = cli.command else {
            panic!("expected recording command");
        };
        assert!(matches!(
            command,
            RecordingCommand::Replay {
                mode: RecordingReplayMode::Verify,
                strict_timing: true,
                max_verify_duration: Some(45),
                verify_start_timeout: Some(20),
                ..
            }
        ));
    }

    #[test]
    fn parses_recording_verify_smoke_flags() {
        let cli = Cli::try_parse_from([
            "bmux",
            "recording",
            "verify-smoke",
            "550e8400-e29b-41d4-a716-446655440000",
            "--strict-timing",
            "--max-verify-duration",
            "50",
            "--verify-start-timeout",
            "30",
        ])
        .expect("valid CLI args");
        let Some(Command::Recording { command }) = cli.command else {
            panic!("expected recording command");
        };
        assert!(matches!(
            command,
            RecordingCommand::VerifySmoke {
                strict_timing: true,
                max_verify_duration: Some(50),
                verify_start_timeout: Some(30),
                ..
            }
        ));
    }

    #[test]
    fn parses_recording_export_flags() {
        let cli = Cli::try_parse_from([
            "bmux",
            "recording",
            "export",
            "550e8400-e29b-41d4-a716-446655440000",
            "--format",
            "gif",
            "--output",
            "./out.gif",
            "--view-client",
            "550e8400-e29b-41d4-a716-446655440001",
            "--fps",
            "15",
            "--speed",
            "1.5",
            "--max-duration",
            "30",
            "--max-frames",
            "250",
            "--renderer",
            "font",
            "--cell-size",
            "9x18",
            "--cell-width",
            "10",
            "--cell-height",
            "20",
            "--font-family",
            "Menlo,Monaco",
            "--font-size",
            "15",
            "--line-height",
            "1.1",
            "--font-path",
            "/tmp/font.ttf",
            "--font-path",
            "/tmp/font2.ttf",
            "--cursor",
            "on",
            "--cursor-shape",
            "bar",
            "--cursor-blink",
            "off",
            "--cursor-blink-period-ms",
            "700",
            "--cursor-color",
            "#11aaee",
            "--cursor-profile",
            "ghostty",
            "--cursor-solid-after-activity-ms",
            "800",
            "--cursor-paint-mode",
            "fill",
            "--cursor-text-mode",
            "swap-fg-bg",
            "--cursor-bar-width-pct",
            "14",
            "--cursor-underline-height-pct",
            "11",
            "--export-metadata",
            "./out.json",
            "--no-progress",
        ])
        .expect("valid CLI args");
        let Some(Command::Recording { command }) = cli.command else {
            panic!("expected recording command");
        };
        assert!(matches!(
            command,
            RecordingCommand::Export {
                format: RecordingExportFormat::Gif,
                fps: 15,
                max_duration: Some(30),
                max_frames: Some(250),
                renderer: RecordingRenderMode::Font,
                cell_size: Some((9, 18)),
                cell_width: Some(10),
                cell_height: Some(20),
                font_family: Some(_),
                font_size: Some(size),
                line_height: Some(line_height),
                font_path,
                cursor: Some(RecordingCursorMode::On),
                cursor_shape: Some(RecordingCursorShape::Bar),
                cursor_blink: Some(RecordingCursorBlinkMode::Off),
                cursor_blink_period_ms: Some(700),
                cursor_color: Some(_),
                cursor_profile: Some(RecordingCursorProfile::Ghostty),
                cursor_solid_after_activity_ms: Some(800),
                cursor_paint_mode: Some(RecordingCursorPaintMode::Fill),
                cursor_text_mode: Some(RecordingCursorTextMode::SwapFgBg),
                cursor_bar_width_pct: Some(14),
                cursor_underline_height_pct: Some(11),
                export_metadata: Some(_),
                no_progress: true,
                ..
            } if (size - 15.0).abs() < f32::EPSILON
                && (line_height - 1.1).abs() < f32::EPSILON
                && font_path.len() == 2
        ));
    }

    #[test]
    fn rejects_invalid_recording_export_cell_size() {
        let error = Cli::try_parse_from([
            "bmux",
            "recording",
            "export",
            "550e8400-e29b-41d4-a716-446655440000",
            "--output",
            "./out.gif",
            "--cell-size",
            "oops",
        ])
        .expect_err("invalid cell-size should fail");
        let text = error.to_string();
        assert!(text.contains("--cell-size") || text.contains("invalid value"));
    }

    #[test]
    fn recording_export_defaults_to_font_renderer() {
        let cli = Cli::try_parse_from([
            "bmux",
            "recording",
            "export",
            "550e8400-e29b-41d4-a716-446655440000",
            "--output",
            "./out.gif",
        ])
        .expect("valid CLI args");
        let Some(Command::Recording { command }) = cli.command else {
            panic!("expected recording command");
        };
        assert!(matches!(
            command,
            RecordingCommand::Export {
                renderer: RecordingRenderMode::Font,
                no_progress: false,
                ..
            }
        ));
    }

    #[test]
    fn parses_top_level_record_flags() {
        let cli = Cli::try_parse_from([
            "bmux",
            "--record",
            "--no-capture-input",
            "--record-profile",
            "visual",
            "--record-event-kind",
            "pane-output-raw",
            "--recording-id-file",
            "/tmp/rec.id",
            "--stop-server-on-exit",
        ])
        .expect("valid CLI args");
        assert!(cli.record);
        assert!(cli.no_capture_input);
        assert_eq!(cli.record_profile, Some(RecordingProfileArg::Visual));
        assert_eq!(
            cli.record_event_kind,
            vec![RecordingEventKindArg::PaneOutputRaw]
        );
        assert_eq!(cli.recording_id_file.as_deref(), Some("/tmp/rec.id"));
        assert!(cli.stop_server_on_exit);
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_recording_start_profile_and_event_kinds() {
        let cli = Cli::try_parse_from([
            "bmux",
            "recording",
            "start",
            "--profile",
            "functional",
            "--event-kind",
            "pane-output-raw",
            "--event-kind",
            "request-start",
        ])
        .expect("valid CLI args");
        let Some(Command::Recording { command }) = cli.command else {
            panic!("expected recording command");
        };
        assert!(matches!(
            command,
            RecordingCommand::Start {
                profile: Some(RecordingProfileArg::Functional),
                event_kind,
                ..
            } if event_kind == vec![
                RecordingEventKindArg::PaneOutputRaw,
                RecordingEventKindArg::RequestStart,
            ]
        ));
    }
}
