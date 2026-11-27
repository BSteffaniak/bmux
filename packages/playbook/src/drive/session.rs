//! Drive sessions: where each lives on disk, its private bmux server, and the
//! typed pane-runtime calls every command makes against the program it runs.
//!
//! A session is a directory under [`drive_home`] holding `session.json`, the
//! recorded script and a private sandbox root. Its server is a daemon, so
//! every `bmux drive` invocation is a short-lived client: nothing else has to
//! stay running between an agent's tool calls.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use bmux_client::BmuxClient;
use bmux_config::ConfigPaths;
use bmux_pane_runtime_plugin_api::{
    pane_runtime_environment_v1 as environment, pane_runtime_execution_v4 as execution,
    pane_runtime_presentation_v1 as presentation, pane_runtime_state as pane_state,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::sandbox;
use crate::types::{PluginConfig, SandboxEnvMode};

const STATE_VERSION: u32 = 1;
/// JSON terminal-grid snapshots, with pointer-shape state.
const GRID_FORMAT_VERSION: u32 = 1;
const SERVER_STARTUP_TIMEOUT: Duration = crate::sandbox::SANDBOX_STARTUP_BOUND;
/// Largest ordered-capture read per request (the pane runtime's limit).
const CAPTURE_READ_BYTES: u32 = 262_144;
/// Most capture bytes any one command reads (lint rescans from the start).
const CAPTURE_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// What `session.json` records about one drive session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub version: u32,
    pub name: String,
    pub command: Vec<String>,
    pub cwd: String,
    /// Current size.
    pub cols: u16,
    pub rows: u16,
    /// Size at start, where replays of the output begin.
    #[serde(default)]
    pub start_cols: u16,
    #[serde(default)]
    pub start_rows: u16,
    pub session_id: Uuid,
    pub pane_id: Uuid,
    pub execution_token: Uuid,
    pub created_at_ms: u64,
    /// Screen rows at the last observation, for "what changed" reports.
    #[serde(default)]
    pub last_rows: Vec<String>,
    /// Style signature of each of those rows.
    #[serde(default)]
    pub last_row_styles: Vec<u64>,
    /// Ordered-capture offset reached by the last command.
    #[serde(default)]
    pub capture_offset: u64,
    /// Protocol findings already reported, so each command reports only new ones.
    #[serde(default)]
    pub reported_findings: Vec<String>,
}

impl SessionRecord {
    pub fn execution(&self) -> execution::TerminalExecutionRef {
        execution::TerminalExecutionRef {
            session_id: self.session_id,
            pane_id: self.pane_id,
            execution_token: self.execution_token,
        }
    }
}

/// Where drive sessions live: `BMUX_DRIVE_HOME`, else a per-user directory
/// in the temp directory. Not the runtime directory: that is often a small
/// memory filesystem, and a session's server keeps runtime images there.
pub fn drive_home() -> PathBuf {
    if let Some(home) = std::env::var_os("BMUX_DRIVE_HOME") {
        return PathBuf::from(home);
    }
    std::env::temp_dir().join(format!("bmux-drive-{}", user_id()))
}

#[cfg(unix)]
fn user_id() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn user_id() -> u32 {
    0
}

pub fn session_dir(name: &str) -> PathBuf {
    drive_home().join(name)
}

fn sandbox_root(dir: &Path) -> PathBuf {
    dir.join("sb")
}

pub fn sandbox_paths(dir: &Path) -> ConfigPaths {
    let root = sandbox_root(dir);
    ConfigPaths::new(root.join("c"), root.join("r"), root.join("d"), root.join("s"))
}

pub fn script_path(dir: &Path) -> PathBuf {
    dir.join("script.drive")
}

/// A session name is a path component an agent can type: letters, digits, `-`, `_`, `.`.
pub fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 48
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid {
        bail!("invalid session name '{name}': use 1-48 letters, digits, '-', '_' or '.'");
    }
    Ok(())
}

/// A free session name derived from the program (`htop`, then `htop-2`, ...).
pub fn default_name(program: &str) -> String {
    let base: String = Path::new(program)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let base = if base.trim_matches('-').is_empty() {
        "term".to_owned()
    } else {
        base.trim_matches('-').to_owned()
    };
    if !session_dir(&base).exists() {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|name| !session_dir(name).exists()).unwrap_or(base)
}

/// Every session directory with a readable record, sorted by name.
pub fn list_sessions() -> Vec<SessionRecord> {
    let Ok(entries) = std::fs::read_dir(drive_home()) else {
        return Vec::new();
    };
    let mut records: Vec<SessionRecord> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| read_record(&entry.path()).ok())
        .collect();
    records.sort_by(|a, b| a.name.cmp(&b.name));
    records
}

pub fn read_record(dir: &Path) -> Result<SessionRecord> {
    let bytes = std::fs::read(dir.join("session.json"))
        .with_context(|| format!("no drive session at {}", dir.display()))?;
    let record: SessionRecord =
        serde_json::from_slice(&bytes).context("unreadable drive session record")?;
    anyhow::ensure!(
        record.version == STATE_VERSION,
        "drive session record version {} is not supported",
        record.version
    );
    Ok(record)
}

pub fn write_record(dir: &Path, record: &SessionRecord) -> Result<()> {
    let tmp = dir.join("session.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(record)?)?;
    std::fs::rename(&tmp, dir.join("session.json"))?;
    Ok(())
}

/// Pick the session a command addresses: the named one, or the only one.
pub fn resolve_session(name: Option<&str>) -> Result<(PathBuf, SessionRecord)> {
    if let Some(name) = name {
        validate_name(name)?;
        let dir = session_dir(name);
        let record = read_record(&dir).with_context(|| {
            format!("no drive session named '{name}' (see `bmux drive list`)")
        })?;
        return Ok((dir, record));
    }
    let mut sessions = list_sessions();
    match sessions.len() {
        0 => bail!("no drive session is running; start one with `bmux drive start -- <program>`"),
        1 => {
            let record = sessions.remove(0);
            Ok((session_dir(&record.name), record))
        }
        _ => {
            let names: Vec<_> = sessions.iter().map(|s| s.name.as_str()).collect();
            bail!(
                "several drive sessions are running ({}); choose one with -s NAME",
                names.join(", ")
            )
        }
    }
}

/// An exclusive lock on a session directory, held for one command so two
/// concurrent tool calls cannot interleave input and observations.
pub struct SessionLock(#[allow(dead_code)] std::fs::File);

pub fn lock_session(dir: &Path) -> Result<SessionLock> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))
        .context("failed opening drive session lock")?;
    file.lock().context("failed locking drive session")?;
    Ok(SessionLock(file))
}

pub async fn connect(dir: &Path) -> Result<BmuxClient> {
    BmuxClient::connect_with_paths(&sandbox_paths(dir), "bmux-drive")
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "the session's bmux server is not reachable ({error}); it may have been stopped. \
                 Remove it with `bmux drive stop` and start again"
            )
        })
}

/// Options for starting a program.
pub struct LaunchOptions {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}

/// Create the session directory and its private daemon server.
pub async fn start_server(dir: &Path, bmux: &Path) -> Result<()> {
    let home = drive_home();
    if !home.exists() {
        sandbox::create_private_directory(&home).context("failed creating drive home")?;
    }
    sandbox::create_private_directory(dir)
        .with_context(|| format!("drive session directory {} already exists", dir.display()))?;
    let root = sandbox_root(dir);
    sandbox::create_private_directory(&root)?;
    let paths = sandbox_paths(dir);
    sandbox::create_private_directory(&paths.runtime_dir)?;
    let plugins = PluginConfig::default();
    let env = BTreeMap::from([(
        "HOME".to_owned(),
        std::env::var("HOME").unwrap_or_else(|_| root.display().to_string()),
    )]);
    sandbox::write_sandbox_config_with_options(
        &paths,
        &sandbox::SandboxStartOptions {
            shell: None,
            input_profile: None,
            plugin_config: &plugins,
            recording_requested: false,
            startup_timeout: SERVER_STARTUP_TIMEOUT,
            env: &env,
            env_mode: SandboxEnvMode::Inherit,
            binary: None,
            sandbox_config_file: None,
            bundled_plugin_ids: &[],
        },
    )?;
    // A program launched directly needs no shell integration (whose callback
    // keeps a copy of the bmux executable for each server).
    let mut config = std::fs::read_to_string(paths.config_file())?;
    config.push_str("\n[behavior]\npane_shell_integration = false\n");
    std::fs::write(paths.config_file(), config)?;
    let logs = root.join("logs");
    std::fs::create_dir_all(&logs)?;
    let mut command = ProcessCommand::new(bmux);
    command.arg("server").arg("start").arg("--daemon");
    sandbox::apply_sandbox_env(&mut command, &paths, &root, &env, SandboxEnvMode::Inherit);
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed starting bmux server with {}", bmux.display()))?;
    std::fs::write(logs.join("server-start.stdout.log"), &output.stdout)?;
    std::fs::write(logs.join("server-start.stderr.log"), &output.stderr)?;
    if !output.status.success() {
        bail!(
            "bmux server failed to start: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    sandbox::wait_for_server_ready(&paths, SERVER_STARTUP_TIMEOUT, None).await
}

/// Stop the session's server and remove its directory. Best effort: a server
/// that is already gone still has its directory removed.
pub async fn stop_server(dir: &Path) -> Result<()> {
    let pid = server_pid(dir);
    if let Ok(mut client) = connect(dir).await {
        let _ = tokio::time::timeout(Duration::from_secs(10), client.stop_server()).await;
    }
    // The server writes its last state while exiting: wait for the process.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while pid.is_some_and(process_alive) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let mut attempt = 0;
    loop {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) if attempt < 20 => {
                attempt += 1;
                tracing::debug!(%error, "retrying drive session removal");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed removing drive session {}", dir.display()));
            }
        }
    }
}

fn server_pid(dir: &Path) -> Option<u32> {
    let meta: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sandbox_paths(dir).runtime_dir.join("server-meta.json")).ok()?,
    )
    .ok()?;
    u32::try_from(meta.get("pid")?.as_u64()?).ok()
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    false
}

/// Launch the program in a new session of the server, with its environment
/// given exactly (the caller's, plus overrides), so it runs as it would in the
/// agent's own shell.
pub async fn launch(
    client: &mut BmuxClient,
    options: LaunchOptions,
) -> Result<execution::TerminalExecutionRef> {
    let program_options = execution::TerminalProgramOptions {
        program: options.program,
        args: options.args,
        cwd: Some(options.cwd.display().to_string()),
        env: BTreeMap::new(),
        cols: options.cols,
        rows: options.rows,
    };
    let mutation = Uuid::new_v4();
    let exact = environment::client::capabilities(client).await.unwrap_or(0) == 2;
    let reservation = if exact {
        environment::client::reserve(
            client,
            mutation,
            program_options,
            program_environment(&options.env),
        )
        .await
    } else {
        let mut program_options = program_options;
        program_options.env = options.env;
        execution::client::reserve(client, mutation, program_options).await
    }
    .map_err(|error| anyhow::anyhow!("reserving the program failed: {error}"))?
    .map_err(|error| anyhow::anyhow!("reserving the program failed: {error:?}"))?;
    let receipt = execution::client::launch(client, reservation)
        .await
        .map_err(|error| anyhow::anyhow!("launching the program failed: {error}"))?
        .map_err(|error| anyhow::anyhow!("launching the program failed: {error:?}"))?;
    if let Some(error) = receipt.error {
        bail!("launching the program failed: {error}");
    }
    Ok(receipt.reservation.execution)
}

/// Variables describing the terminal or multiplexer pane the caller runs in,
/// which do not describe the program's terminal.
const CALLER_TERMINAL_VARIABLES: &[&str] = &[
    "COLUMNS",
    "LINES",
    "TMUX",
    "TMUX_PANE",
    "BMUX_PANE",
    "BMUX_SHELL_MARKER",
    "BMUX_SHELL_CALLBACK",
    "BMUX_ROOT_SHELL_PID",
    "BMUX_IMAGE_PROTOCOL",
    "__BMUX_NESTED_PID",
    "APPLICATION_RESURRECTION_HOST",
];

fn inherited(key: &std::ffi::OsStr) -> bool {
    !CALLER_TERMINAL_VARIABLES.contains(&key.to_string_lossy().as_ref())
}

/// The caller's environment without its own terminal's identity, overlaid
/// with the terminal the pane emulates and the explicit overrides.
#[cfg(unix)]
fn program_environment(overrides: &BTreeMap<String, String>) -> Vec<environment::EnvironmentEntry> {
    use std::os::unix::ffi::OsStringExt as _;
    let mut env: BTreeMap<std::ffi::OsString, std::ffi::OsString> = std::env::vars_os()
        .filter(|(key, _)| inherited(key))
        .collect();
    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("COLORTERM".into(), "truecolor".into());
    for (key, value) in overrides {
        env.insert(key.into(), value.into());
    }
    env.into_iter()
        .map(|(key, value)| environment::EnvironmentEntry {
            key: key.into_vec(),
            value: value.into_vec(),
            encoding: Some("unix-bytes".into()),
        })
        .collect()
}

#[cfg(windows)]
fn program_environment(overrides: &BTreeMap<String, String>) -> Vec<environment::EnvironmentEntry> {
    use std::os::windows::ffi::OsStrExt as _;
    let mut env: BTreeMap<std::ffi::OsString, std::ffi::OsString> = std::env::vars_os()
        .filter(|(key, _)| inherited(key))
        .collect();
    env.insert("TERM".into(), "xterm-256color".into());
    for (key, value) in overrides {
        env.insert(key.into(), value.into());
    }
    env.into_iter()
        .map(|(key, value)| environment::EnvironmentEntry {
            key: key.encode_wide().flat_map(u16::to_le_bytes).collect(),
            value: value.encode_wide().flat_map(u16::to_le_bytes).collect(),
            encoding: Some("windows-utf16le".into()),
        })
        .collect()
}

pub async fn status(
    client: &mut BmuxClient,
    record: &SessionRecord,
) -> Result<execution::TerminalExecutionStatus> {
    execution::client::status(client, record.execution())
        .await
        .map_err(|error| anyhow::anyhow!("program status unavailable: {error}"))?
        .map_err(|error| anyhow::anyhow!("program status unavailable: {error:?}"))
}

pub async fn screen(
    client: &mut BmuxClient,
    record: &SessionRecord,
) -> Result<execution::TerminalExecutionScreen> {
    // The pointer-aware representation: the plain one refuses screens whose
    // program (or an attached client) set mouse pointer shapes.
    bmux_pane_runtime_plugin_api::pane_runtime_pointer_grid_v2::client::screen(
        client,
        GRID_FORMAT_VERSION,
        record.execution(),
    )
    .await
    .map_err(|error| anyhow::anyhow!("screen unavailable: {error}"))?
    .map_err(|error| anyhow::anyhow!("screen unavailable: {error:?}"))
}

pub async fn send(client: &mut BmuxClient, record: &SessionRecord, data: Vec<u8>) -> Result<()> {
    execution::client::send(client, record.execution(), data)
        .await
        .map_err(|error| anyhow::anyhow!("sending input failed: {error}"))?
        .map_err(|error| match error {
            execution::TerminalExecutionError::StaleExecution
            | execution::TerminalExecutionError::NotFound => {
                anyhow::anyhow!("the program has exited; input was not delivered")
            }
            other => anyhow::anyhow!("sending input failed: {other:?}"),
        })
}

pub async fn resize(
    client: &mut BmuxClient,
    record: &SessionRecord,
    cols: u16,
    rows: u16,
) -> Result<()> {
    execution::client::resize(client, record.execution(), cols, rows)
        .await
        .map_err(|error| anyhow::anyhow!("resize failed: {error}"))?
        .map_err(|error| anyhow::anyhow!("resize failed: {error:?}"))
}

pub async fn terminate(
    client: &mut BmuxClient,
    record: &SessionRecord,
    grace: Duration,
) -> Result<execution::TerminalExecutionStatus> {
    execution::client::terminate(
        client,
        record.execution(),
        u32::try_from(grace.as_millis()).unwrap_or(u32::MAX),
    )
    .await
    .map_err(|error| anyhow::anyhow!("stopping the program failed: {error}"))?
    .map_err(|error| anyhow::anyhow!("stopping the program failed: {error:?}"))
}

/// The pane program's process id, when the pane runtime knows it.
pub async fn program_pid(client: &mut BmuxClient, record: &SessionRecord) -> Option<u32> {
    pane_state::client::get_pane_process(client, record.session_id, record.pane_id)
        .await
        .ok()?
        .ok()?
        .pid
}

/// What the program signalled beyond its screen (title, bells, OSC 133 status).
pub async fn signals(
    client: &mut BmuxClient,
    record: &SessionRecord,
) -> Option<pane_state::PaneSignals> {
    pane_state::client::list_pane_signals(client)
        .await
        .ok()?
        .ok()?
        .into_iter()
        .find(|signals| signals.pane_id == record.pane_id)
}

/// One event of the program's ordered output capture.
#[derive(Debug, Clone)]
pub enum CaptureEvent {
    Output { data: Vec<u8> },
    Resize { cols: u16, rows: u16 },
}

/// Ordered capture events from `offset` (a frame boundary), the offset after
/// the last whole frame read, and whether the read stopped at the size bound.
pub async fn capture_from(
    client: &mut BmuxClient,
    record: &SessionRecord,
    start: u64,
) -> Result<(Vec<CaptureEvent>, u64, bool)> {
    let mut offset = start;
    let mut bytes = Vec::new();
    let mut truncated = false;
    loop {
        let batch = presentation::client::capture_events(
            client,
            record.execution(),
            presentation::TerminalCaptureFormat::OrderedEventsV1,
            offset,
            CAPTURE_READ_BYTES,
        )
        .await
        .map_err(|error| anyhow::anyhow!("output capture unavailable: {error}"))?
        .map_err(|error| anyhow::anyhow!("output capture unavailable: {error:?}"))?;
        let done = batch.data.is_empty() || batch.eof;
        offset = batch.continuation;
        bytes.extend_from_slice(&batch.data);
        if done {
            break;
        }
        if bytes.len() >= CAPTURE_TOTAL_BYTES {
            truncated = true;
            break;
        }
    }
    let (events, consumed) = decode_capture(&bytes)?;
    Ok((events, start + consumed as u64, truncated))
}

/// Decode whole frames; a frame cut off at the end is left for the next read.
fn decode_capture(bytes: &[u8]) -> Result<(Vec<CaptureEvent>, usize)> {
    use presentation::{TerminalCaptureEvent, TerminalCaptureEventKind};
    let mut events = Vec::new();
    let mut consumed = 0;
    while bytes.len() - consumed >= 4 {
        let rest = &bytes[consumed..];
        let length = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        if rest.len() < 4 + length {
            break;
        }
        let event: TerminalCaptureEvent = serde_json::from_slice(&rest[4..4 + length])
            .context("undecodable output capture frame")?;
        events.push(match event.event {
            TerminalCaptureEventKind::Output { data } => CaptureEvent::Output { data },
            TerminalCaptureEventKind::Resize { cols, rows } => CaptureEvent::Resize { cols, rows },
        });
        consumed += 4 + length;
    }
    Ok((events, consumed))
}

/// Resolve the program the way the caller's shell would: a path with a
/// separator relative to `cwd`, otherwise a search of the caller's `PATH`.
pub fn resolve_program(program: &str, cwd: &Path) -> Result<String> {
    let path = Path::new(program);
    if path.components().count() > 1 || path.is_absolute() {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        anyhow::ensure!(resolved.exists(), "program not found: {}", resolved.display());
        return Ok(resolved.display().to_string());
    }
    let search = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&search) {
        let candidate = if dir.is_absolute() {
            dir.join(program)
        } else {
            cwd.join(dir).join(program)
        };
        if is_executable(&candidate) {
            return Ok(candidate.display().to_string());
        }
    }
    bail!("program '{program}' was not found on PATH")
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file() || path.with_extension("exe").is_file()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

pub const fn state_version() -> u32 {
    STATE_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_path_components() {
        assert!(validate_name("htop-2").is_ok());
        assert!(validate_name("my_app.v1").is_ok());
        assert!(validate_name("../x").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name(".hidden").is_err());
        assert!(validate_name("a b").is_err());
    }

    #[test]
    fn capture_frames_decode_in_order() {
        use presentation::{TerminalCaptureEvent, TerminalCaptureEventKind};
        let mut bytes = Vec::new();
        for event in [
            TerminalCaptureEvent {
                mono_ns: 1,
                event: TerminalCaptureEventKind::Output { data: b"hi".to_vec() },
            },
            TerminalCaptureEvent {
                mono_ns: 2,
                event: TerminalCaptureEventKind::Resize { cols: 9, rows: 4 },
            },
        ] {
            let frame = serde_json::to_vec(&event).unwrap();
            bytes.extend_from_slice(&u32::try_from(frame.len()).unwrap().to_le_bytes());
            bytes.extend_from_slice(&frame);
        }
        let (events, consumed) = decode_capture(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert!(matches!(&events[0], CaptureEvent::Output { data } if data == b"hi"));
        assert!(matches!(events[1], CaptureEvent::Resize { cols: 9, rows: 4 }));
        // A frame cut off at the end waits for the next read.
        let (events, consumed) = decode_capture(&bytes[..bytes.len() - 3]).unwrap();
        assert_eq!(events.len(), 1);
        assert!(consumed < bytes.len());
    }
}
