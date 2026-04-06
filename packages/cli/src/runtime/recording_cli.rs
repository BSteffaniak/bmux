use anyhow::{Context, Result};
use bmux_cli_schema::{
    RecordingCursorBlinkMode, RecordingCursorMode, RecordingCursorPaintMode,
    RecordingCursorProfile, RecordingCursorShape, RecordingCursorTextMode, RecordingEventKindArg,
    RecordingExportFormat, RecordingProfileArg, RecordingRenderMode, RecordingReplayMode,
};
use bmux_client::BmuxClient;
use bmux_config::{
    BmuxConfig, ConfigPaths, RecordingExportCursorBlinkMode, RecordingExportCursorMode,
    RecordingExportCursorPaintMode, RecordingExportCursorProfile, RecordingExportCursorShape,
    RecordingExportCursorTextMode,
};
use bmux_ipc::{RecordingEventEnvelope, RecordingEventKind, RecordingPayload, SessionSelector};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::warn;
use uuid::Uuid;

use super::{
    ConnectionContext, VERIFY_SERVER_START_TIMEOUT_DEFAULT, bundled_plugin_roots,
    map_cli_client_error, parse_pid_content, recording, registered_plugin_entry_exists,
    scan_available_plugins, try_kill_pid,
};

pub(super) async fn run_recording_start(
    session_id: Option<&str>,
    capture_input: bool,
    profile: Option<RecordingProfileArg>,
    event_kinds: &[RecordingEventKindArg],
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    recording::run_recording_start(
        session_id,
        capture_input,
        profile,
        event_kinds,
        connection_context,
    )
    .await
}

pub(super) async fn run_recording_stop(
    recording_id: Option<&str>,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    recording::run_recording_stop(recording_id, connection_context).await
}

pub(super) async fn run_recording_status(
    as_json: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    recording::run_recording_status(as_json, connection_context).await
}

pub(super) fn run_recording_path(as_json: bool) -> Result<u8> {
    recording::run_recording_path(as_json)
}

pub(super) async fn run_recording_list(
    as_json: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    recording::run_recording_list(as_json, connection_context).await
}

pub(super) async fn run_recording_delete(
    recording_id_or_prefix: &str,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    recording::run_recording_delete(recording_id_or_prefix, connection_context).await
}

pub(super) async fn run_recording_delete_all(
    yes: bool,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    recording::run_recording_delete_all(yes, connection_context).await
}

pub(super) async fn run_recording_cut(
    last_seconds: Option<u64>,
    connection_context: ConnectionContext<'_>,
) -> Result<u8> {
    recording::run_recording_cut(last_seconds, connection_context).await
}

pub(super) fn run_recording_inspect(
    recording_id: &str,
    limit: usize,
    kind: Option<&str>,
    as_json: bool,
) -> Result<u8> {
    recording::run_recording_inspect(recording_id, limit, kind, as_json)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_recording_replay(
    recording_id: &str,
    mode: RecordingReplayMode,
    speed: f64,
    target_bmux: Option<&str>,
    compare_recording: Option<&str>,
    ignore: Option<&str>,
    strict_timing: bool,
    max_verify_duration_secs: Option<u64>,
    verify_start_timeout_secs: Option<u64>,
) -> Result<u8> {
    recording::run_recording_replay(
        recording_id,
        mode,
        speed,
        target_bmux,
        compare_recording,
        ignore,
        strict_timing,
        max_verify_duration_secs,
        verify_start_timeout_secs,
    )
    .await
}

pub(super) async fn run_recording_verify_smoke(
    recording_id: &str,
    target_bmux: Option<&str>,
    compare_recording: Option<&str>,
    ignore: Option<&str>,
    strict_timing: bool,
    max_verify_duration_secs: Option<u64>,
    verify_start_timeout_secs: Option<u64>,
) -> Result<u8> {
    recording::run_recording_verify_smoke(
        recording_id,
        target_bmux,
        compare_recording,
        ignore,
        strict_timing,
        max_verify_duration_secs,
        verify_start_timeout_secs,
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub(super) async fn run_recording_export(
    recording_id: &str,
    format: RecordingExportFormat,
    output: &str,
    view_client: Option<&str>,
    speed: f64,
    fps: u32,
    max_duration: Option<u64>,
    max_frames: Option<u32>,
    renderer: RecordingRenderMode,
    cell_size: Option<(u16, u16)>,
    cell_width: Option<u16>,
    cell_height: Option<u16>,
    font_family: Option<&str>,
    font_size: Option<f32>,
    line_height: Option<f32>,
    font_path: &[String],
    cursor: Option<RecordingCursorMode>,
    cursor_shape: Option<RecordingCursorShape>,
    cursor_blink: Option<RecordingCursorBlinkMode>,
    cursor_blink_period_ms: Option<u32>,
    cursor_color: Option<&str>,
    cursor_profile: Option<RecordingCursorProfile>,
    cursor_solid_after_activity_ms: Option<u32>,
    cursor_solid_after_input_ms: Option<u32>,
    cursor_solid_after_output_ms: Option<u32>,
    cursor_solid_after_cursor_ms: Option<u32>,
    cursor_paint_mode: Option<RecordingCursorPaintMode>,
    cursor_text_mode: Option<RecordingCursorTextMode>,
    cursor_bar_width_pct: Option<u8>,
    cursor_underline_height_pct: Option<u8>,
    export_metadata: Option<&str>,
    show_progress: bool,
) -> Result<u8> {
    let paths = ConfigPaths::default();
    let config = BmuxConfig::load_from_path(&paths.config_file()).unwrap_or_default();
    let export_defaults = &config.recording.export;

    let resolved_cursor = cursor.unwrap_or(match export_defaults.cursor {
        RecordingExportCursorMode::Auto => RecordingCursorMode::Auto,
        RecordingExportCursorMode::On => RecordingCursorMode::On,
        RecordingExportCursorMode::Off => RecordingCursorMode::Off,
    });
    let resolved_cursor_shape = cursor_shape.unwrap_or(match export_defaults.cursor_shape {
        RecordingExportCursorShape::Auto => RecordingCursorShape::Auto,
        RecordingExportCursorShape::Block => RecordingCursorShape::Block,
        RecordingExportCursorShape::Bar => RecordingCursorShape::Bar,
        RecordingExportCursorShape::Underline => RecordingCursorShape::Underline,
    });
    let resolved_cursor_blink = cursor_blink.unwrap_or(match export_defaults.cursor_blink {
        RecordingExportCursorBlinkMode::Auto => RecordingCursorBlinkMode::Auto,
        RecordingExportCursorBlinkMode::On => RecordingCursorBlinkMode::On,
        RecordingExportCursorBlinkMode::Off => RecordingCursorBlinkMode::Off,
    });
    let resolved_cursor_blink_period_ms =
        cursor_blink_period_ms.unwrap_or_else(|| export_defaults.cursor_blink_period_ms.max(1));
    let resolved_cursor_color = cursor_color
        .map(str::to_string)
        .or_else(|| {
            let value = export_defaults.cursor_color.trim();
            (!value.is_empty()).then(|| value.to_string())
        })
        .unwrap_or_else(|| "auto".to_string());
    let resolved_cursor_profile = cursor_profile.unwrap_or(match export_defaults.cursor_profile {
        RecordingExportCursorProfile::Auto => RecordingCursorProfile::Auto,
        RecordingExportCursorProfile::Ghostty => RecordingCursorProfile::Ghostty,
        RecordingExportCursorProfile::Generic => RecordingCursorProfile::Generic,
    });
    let resolved_cursor_solid_after_activity_ms =
        cursor_solid_after_activity_ms.or(export_defaults.cursor_solid_after_activity_ms);
    let resolved_cursor_solid_after_input_ms =
        cursor_solid_after_input_ms.or(export_defaults.cursor_solid_after_input_ms);
    let resolved_cursor_solid_after_output_ms =
        cursor_solid_after_output_ms.or(export_defaults.cursor_solid_after_output_ms);
    let resolved_cursor_solid_after_cursor_ms =
        cursor_solid_after_cursor_ms.or(export_defaults.cursor_solid_after_cursor_ms);
    let resolved_cursor_paint_mode =
        cursor_paint_mode.unwrap_or(match export_defaults.cursor_paint_mode {
            RecordingExportCursorPaintMode::Auto => RecordingCursorPaintMode::Auto,
            RecordingExportCursorPaintMode::Invert => RecordingCursorPaintMode::Invert,
            RecordingExportCursorPaintMode::Fill => RecordingCursorPaintMode::Fill,
            RecordingExportCursorPaintMode::Outline => RecordingCursorPaintMode::Outline,
        });
    let resolved_cursor_text_mode =
        cursor_text_mode.unwrap_or(match export_defaults.cursor_text_mode {
            RecordingExportCursorTextMode::Auto => RecordingCursorTextMode::Auto,
            RecordingExportCursorTextMode::SwapFgBg => RecordingCursorTextMode::SwapFgBg,
            RecordingExportCursorTextMode::ForceContrast => RecordingCursorTextMode::ForceContrast,
        });
    let resolved_cursor_bar_width_pct =
        cursor_bar_width_pct.unwrap_or_else(|| export_defaults.cursor_bar_width_pct.clamp(1, 100));
    let resolved_cursor_underline_height_pct = cursor_underline_height_pct
        .unwrap_or_else(|| export_defaults.cursor_underline_height_pct.clamp(1, 100));

    recording::run_recording_export(
        recording_id,
        format,
        output,
        view_client,
        speed,
        fps,
        max_duration,
        max_frames,
        renderer,
        cell_size,
        cell_width,
        cell_height,
        font_family,
        font_size,
        line_height,
        font_path,
        resolved_cursor,
        resolved_cursor_shape,
        resolved_cursor_blink,
        resolved_cursor_blink_period_ms,
        &resolved_cursor_color,
        resolved_cursor_profile,
        resolved_cursor_solid_after_activity_ms,
        resolved_cursor_solid_after_input_ms,
        resolved_cursor_solid_after_output_ms,
        resolved_cursor_solid_after_cursor_ms,
        resolved_cursor_paint_mode,
        resolved_cursor_text_mode,
        resolved_cursor_bar_width_pct,
        resolved_cursor_underline_height_pct,
        export_metadata,
        show_progress,
    )
    .await
}

const REPLAY_SPEED_MIN: f64 = 0.125;
const REPLAY_SPEED_MAX: f64 = 32.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractiveReplayAction {
    TogglePause,
    Step,
    SlowDown,
    SpeedUp,
    OpenShell,
    Quit,
}

#[derive(Debug, Clone, Copy)]
struct InteractiveReplayState {
    paused: bool,
    speed: f64,
}

impl InteractiveReplayState {
    fn new(speed: f64) -> Self {
        Self {
            paused: false,
            speed: normalize_replay_speed(speed),
        }
    }
}

struct ReplayTimeline<'a> {
    events: &'a [RecordingEventEnvelope],
    next_index: usize,
    last_ns: u64,
}

impl<'a> ReplayTimeline<'a> {
    fn new(events: &'a [RecordingEventEnvelope]) -> Self {
        let last_ns = events.first().map_or(0, |event| event.mono_ns);
        Self {
            events,
            next_index: 0,
            last_ns,
        }
    }

    const fn is_finished(&self) -> bool {
        self.next_index >= self.events.len()
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn next_delay(&self, speed: f64) -> Duration {
        let Some(event) = self.events.get(self.next_index) else {
            return Duration::ZERO;
        };
        if event.mono_ns <= self.last_ns {
            return Duration::ZERO;
        }
        let delta = event.mono_ns.saturating_sub(self.last_ns);
        Duration::from_nanos(((delta as f64) / normalize_replay_speed(speed)) as u64)
    }

    fn advance(&mut self, stdout: &mut impl Write) -> Result<bool> {
        let Some(event) = self.events.get(self.next_index) else {
            return Ok(false);
        };
        let wrote_bytes = write_replay_event(stdout, event)?;
        self.last_ns = event.mono_ns;
        self.next_index = self.next_index.saturating_add(1);
        Ok(wrote_bytes)
    }

    fn step_to_next_output(&mut self, stdout: &mut impl Write) -> Result<bool> {
        while !self.is_finished() {
            if self.advance(stdout)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

pub(super) fn replay_watch(events: &[RecordingEventEnvelope], speed: f64) -> Result<u8> {
    let mut timeline = ReplayTimeline::new(events);
    let mut stdout = io::stdout().lock();

    while !timeline.is_finished() {
        let delay = timeline.next_delay(speed);
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let _ = timeline.advance(&mut stdout)?;
    }

    stdout.flush()?;
    Ok(0)
}

pub(super) fn replay_interactive(events: &[RecordingEventEnvelope], speed: f64) -> Result<u8> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!("interactive replay requires a TTY on stdin/stdout")
    }

    let _raw_mode_guard = ReplayRawModeGuard::enable()?;
    eprintln!(
        "interactive replay controls: space pause/resume | . step | [ slower | ] faster | ! shell | q quit"
    );

    let mut stdout = io::stdout().lock();
    let mut timeline = ReplayTimeline::new(events);
    let mut state = InteractiveReplayState::new(speed);

    while !timeline.is_finished() {
        let action = if state.paused {
            read_interactive_replay_action_blocking()?
        } else {
            read_interactive_replay_action_timeout(timeline.next_delay(state.speed))?
        };

        if let Some(action) = action {
            match action {
                InteractiveReplayAction::TogglePause => {
                    state.paused = !state.paused;
                    eprintln!("replay {}", if state.paused { "paused" } else { "resumed" });
                }
                InteractiveReplayAction::Step => {
                    state.paused = true;
                    let wrote_output = timeline.step_to_next_output(&mut stdout)?;
                    stdout.flush()?;
                    if !wrote_output {
                        eprintln!("replay complete");
                        break;
                    }
                    eprintln!("replay stepped");
                }
                InteractiveReplayAction::SlowDown => {
                    state.speed = normalize_replay_speed((state.speed / 2.0).max(REPLAY_SPEED_MIN));
                    eprintln!("replay speed {:.3}x", state.speed);
                }
                InteractiveReplayAction::SpeedUp => {
                    state.speed = normalize_replay_speed((state.speed * 2.0).min(REPLAY_SPEED_MAX));
                    eprintln!("replay speed {:.3}x", state.speed);
                }
                InteractiveReplayAction::OpenShell => {
                    state.paused = true;
                    stdout
                        .flush()
                        .context("failed flushing replay output before shell")?;
                    eprintln!("replay shell: type 'exit' to return");
                    open_replay_shell()?;
                    eprintln!("replay shell closed (paused)");
                }
                InteractiveReplayAction::Quit => {
                    eprintln!("replay stopped");
                    return Ok(0);
                }
            }
            continue;
        }

        if state.paused {
            continue;
        }

        let _ = timeline.advance(&mut stdout)?;
        stdout.flush()?;
    }

    Ok(0)
}

struct ReplayRawModeGuard;

impl ReplayRawModeGuard {
    fn enable() -> Result<Self> {
        enable_raw_mode().context("failed enabling raw mode for interactive replay")?;
        Ok(Self)
    }
}

impl Drop for ReplayRawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

fn normalize_replay_speed(speed: f64) -> f64 {
    if !speed.is_finite() || speed <= 0.0 {
        return 1.0;
    }
    speed.clamp(REPLAY_SPEED_MIN, REPLAY_SPEED_MAX)
}

const fn replay_action_from_key_event(key: KeyEvent) -> Option<InteractiveReplayAction> {
    replay_action_from_key(key.code, key.modifiers, key.kind)
}

const fn replay_action_from_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    kind: KeyEventKind,
) -> Option<InteractiveReplayAction> {
    if !matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c' | 'd')) {
        return Some(InteractiveReplayAction::Quit);
    }

    match code {
        KeyCode::Char(' ') => Some(InteractiveReplayAction::TogglePause),
        KeyCode::Char('.') => Some(InteractiveReplayAction::Step),
        KeyCode::Char('[') => Some(InteractiveReplayAction::SlowDown),
        KeyCode::Char(']') => Some(InteractiveReplayAction::SpeedUp),
        KeyCode::Char('!') => Some(InteractiveReplayAction::OpenShell),
        KeyCode::Char('q') | KeyCode::Esc => Some(InteractiveReplayAction::Quit),
        _ => None,
    }
}

fn read_interactive_replay_action_timeout(
    timeout: Duration,
) -> Result<Option<InteractiveReplayAction>> {
    if timeout.is_zero() {
        return read_interactive_replay_action_poll(Duration::ZERO);
    }

    let started = Instant::now();
    while started.elapsed() < timeout {
        let remaining = timeout.saturating_sub(started.elapsed());
        if let Some(action) = read_interactive_replay_action_poll(remaining)? {
            return Ok(Some(action));
        }
        if started.elapsed() >= timeout {
            return Ok(None);
        }
    }

    Ok(None)
}

fn read_interactive_replay_action_blocking() -> Result<Option<InteractiveReplayAction>> {
    loop {
        if let Event::Key(key) = crossterm::event::read().context("failed reading replay input")?
            && let Some(action) = replay_action_from_key_event(key)
        {
            return Ok(Some(action));
        }
    }
}

fn read_interactive_replay_action_poll(
    timeout: Duration,
) -> Result<Option<InteractiveReplayAction>> {
    if !crossterm::event::poll(timeout).context("failed polling replay input")? {
        return Ok(None);
    }

    let event = crossterm::event::read().context("failed reading replay input")?;
    match event {
        Event::Key(key) => Ok(replay_action_from_key_event(key)),
        _ => Ok(None),
    }
}

fn open_replay_shell() -> Result<()> {
    let shell = std::env::var("SHELL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string());

    disable_raw_mode().context("failed disabling raw mode for replay shell")?;
    let status = ProcessCommand::new(&shell)
        .status()
        .with_context(|| format!("failed launching replay shell '{shell}'"));
    enable_raw_mode().context("failed re-enabling raw mode after replay shell")?;

    status?;
    Ok(())
}

fn write_replay_event(stdout: &mut impl Write, event: &RecordingEventEnvelope) -> Result<bool> {
    match &event.payload {
        RecordingPayload::Bytes { data }
            if matches!(
                event.kind,
                RecordingEventKind::PaneOutputRaw | RecordingEventKind::ProtocolReplyRaw
            ) =>
        {
            stdout.write_all(data)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct VerifySmokeReport {
    pub(super) pass: bool,
    pub(super) reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_binary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compare_recording: Option<String>,
    strict_timing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_verify_duration_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verify_start_timeout_secs: Option<u64>,
    ignored_kinds: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mismatch_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_output_len: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_output_len: Option<usize>,
    monotonic_timeline: bool,
}

pub(super) async fn replay_verify(
    baseline: &[RecordingEventEnvelope],
    target_bmux: Option<&str>,
    compare_recording: Option<&str>,
    ignore: Option<&str>,
    strict_timing: bool,
    max_verify_duration_secs: Option<u64>,
    verify_start_timeout_secs: Option<u64>,
) -> Result<u8> {
    let report = verify_recording_report(
        baseline,
        target_bmux,
        compare_recording,
        ignore,
        strict_timing,
        max_verify_duration_secs,
        verify_start_timeout_secs,
    )
    .await?;

    if let Some(target_binary) = &report.target_binary {
        println!("verify target binary: {target_binary}");
    }

    if report.pass {
        println!("verify PASS: {}", report.reason);
        return Ok(0);
    }

    if let (Some(index), Some(expected), Some(actual), Some(expected_kind), Some(actual_kind)) = (
        report.mismatch_index,
        report.expected_seq,
        report.actual_seq,
        report.expected_kind.as_ref(),
        report.actual_kind.as_ref(),
    ) {
        println!(
            "verify FAIL: mismatch at index {index} expected_seq={expected} actual_seq={actual} expected_kind={expected_kind} actual_kind={actual_kind}"
        );
        return Ok(1);
    }
    if let (Some(expected), Some(actual)) = (report.expected_output_len, report.actual_output_len) {
        println!("verify FAIL: output length mismatch expected={expected} actual={actual}");
        return Ok(1);
    }
    println!("verify FAIL: {}", report.reason);
    Ok(1)
}

#[allow(clippy::too_many_lines)]
pub(super) async fn verify_recording_report(
    baseline: &[RecordingEventEnvelope],
    target_bmux: Option<&str>,
    compare_recording: Option<&str>,
    ignore: Option<&str>,
    strict_timing: bool,
    max_verify_duration_secs: Option<u64>,
    verify_start_timeout_secs: Option<u64>,
) -> Result<VerifySmokeReport> {
    let ignore_rules = parse_ignore_rules(ignore);
    let baseline_filtered = apply_ignore_rules(baseline, &ignore_rules);
    if let Some(other_id) = compare_recording {
        let other = load_recording_events(other_id)?;
        let other_filtered = apply_ignore_rules(&other, &ignore_rules);
        let mismatch = baseline_filtered
            .iter()
            .zip(other_filtered.iter())
            .position(|(left, right)| left != right);
        if let Some(index) = mismatch {
            let expected = &baseline_filtered[index];
            let actual = &other_filtered[index];
            return Ok(VerifySmokeReport {
                pass: false,
                reason: "recordings diverged".to_string(),
                target_binary: None,
                compare_recording: Some(other_id.to_string()),
                strict_timing,
                max_verify_duration_secs,
                verify_start_timeout_secs,
                ignored_kinds: ignore_rules,
                mismatch_index: Some(index),
                expected_seq: Some(expected.seq),
                actual_seq: Some(actual.seq),
                expected_kind: Some(recording_event_kind_name(expected.kind)),
                actual_kind: Some(recording_event_kind_name(actual.kind)),
                expected_output_len: Some(baseline_filtered.len()),
                actual_output_len: Some(other_filtered.len()),
                monotonic_timeline: true,
            });
        }
        if baseline_filtered.len() != other_filtered.len() {
            return Ok(VerifySmokeReport {
                pass: false,
                reason: "recordings length mismatch".to_string(),
                target_binary: None,
                compare_recording: Some(other_id.to_string()),
                strict_timing,
                max_verify_duration_secs,
                verify_start_timeout_secs,
                ignored_kinds: ignore_rules,
                mismatch_index: None,
                expected_seq: None,
                actual_seq: None,
                expected_kind: None,
                actual_kind: None,
                expected_output_len: Some(baseline_filtered.len()),
                actual_output_len: Some(other_filtered.len()),
                monotonic_timeline: true,
            });
        }
        return Ok(VerifySmokeReport {
            pass: true,
            reason: "recordings are identical".to_string(),
            target_binary: None,
            compare_recording: Some(other_id.to_string()),
            strict_timing,
            max_verify_duration_secs,
            verify_start_timeout_secs,
            ignored_kinds: ignore_rules,
            mismatch_index: None,
            expected_seq: None,
            actual_seq: None,
            expected_kind: None,
            actual_kind: None,
            expected_output_len: Some(baseline_filtered.len()),
            actual_output_len: Some(other_filtered.len()),
            monotonic_timeline: true,
        });
    }

    let target_binary = match target_bmux {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe().context("failed resolving current bmux binary")?,
    };
    let input_timeline = input_timeline(&baseline_filtered);
    let first_input_ns = input_timeline.first().map(|event| event.mono_ns);
    let expected_output = first_input_ns.map_or_else(Vec::new, |min_ns| {
        expected_output_bytes(&baseline_filtered, Some(min_ns))
    });
    // Extract viewport dimensions from recording (first AttachSetViewport request).
    let viewport = extract_viewport_from_events(&baseline_filtered);
    let actual_output = run_target_verify_capture(
        &target_binary,
        &input_timeline,
        strict_timing,
        max_verify_duration_secs,
        verify_start_timeout_secs,
        viewport,
    )
    .await?;

    // Compare output: first try byte-exact, then fall back to structural
    // (vt100-rendered) comparison which tolerates byte-level differences from
    // timing/chunking while catching actual content divergence.
    let byte_mismatch = expected_output
        .iter()
        .zip(actual_output.iter())
        .position(|(left, right)| left != right);
    let length_mismatch = expected_output.len() != actual_output.len();

    if byte_mismatch.is_some() || length_mismatch {
        // Byte comparison failed — try structural comparison via vt100.
        let (vp_cols, vp_rows) = viewport.unwrap_or((120, 40));
        let expected_text = render_output_via_vt100(&expected_output, vp_cols, vp_rows);
        let actual_text = render_output_via_vt100(&actual_output, vp_cols, vp_rows);

        // Normalize both: collapse digit sequences, strip trailing whitespace.
        let expected_norm = normalize_screen_text(&expected_text);
        let actual_norm = normalize_screen_text(&actual_text);

        if expected_norm != actual_norm {
            let mismatch_detail = find_text_mismatch(&expected_norm, &actual_norm);
            return Ok(VerifySmokeReport {
                pass: false,
                reason: format!("output mismatch (structural comparison): {mismatch_detail}"),
                target_binary: Some(target_binary.display().to_string()),
                compare_recording: None,
                strict_timing,
                max_verify_duration_secs,
                verify_start_timeout_secs,
                ignored_kinds: ignore_rules,
                mismatch_index: byte_mismatch,
                expected_seq: None,
                actual_seq: None,
                expected_kind: None,
                actual_kind: None,
                expected_output_len: Some(expected_output.len()),
                actual_output_len: Some(actual_output.len()),
                monotonic_timeline: true,
            });
        }
        // Structural comparison passed — byte differences were cosmetic.
    }

    let monotonic = baseline_filtered
        .windows(2)
        .all(|pair| pair[1].seq > pair[0].seq && pair[1].mono_ns >= pair[0].mono_ns);
    if !monotonic {
        return Ok(VerifySmokeReport {
            pass: false,
            reason: "non-monotonic sequence or timestamp ordering".to_string(),
            target_binary: Some(target_binary.display().to_string()),
            compare_recording: None,
            strict_timing,
            max_verify_duration_secs,
            verify_start_timeout_secs,
            ignored_kinds: ignore_rules,
            mismatch_index: None,
            expected_seq: None,
            actual_seq: None,
            expected_kind: None,
            actual_kind: None,
            expected_output_len: Some(expected_output.len()),
            actual_output_len: Some(actual_output.len()),
            monotonic_timeline: false,
        });
    }
    Ok(VerifySmokeReport {
        pass: true,
        reason: "target output and timeline integrity checks succeeded".to_string(),
        target_binary: Some(target_binary.display().to_string()),
        compare_recording: None,
        strict_timing,
        max_verify_duration_secs,
        verify_start_timeout_secs,
        ignored_kinds: ignore_rules,
        mismatch_index: None,
        expected_seq: None,
        actual_seq: None,
        expected_kind: None,
        actual_kind: None,
        expected_output_len: Some(expected_output.len()),
        actual_output_len: Some(actual_output.len()),
        monotonic_timeline: true,
    })
}

#[derive(Debug, Clone)]
pub(super) struct ReplayInputEvent {
    mono_ns: u64,
    data: Vec<u8>,
}

pub(super) fn expected_output_bytes(
    events: &[RecordingEventEnvelope],
    min_mono_ns: Option<u64>,
) -> Vec<u8> {
    let mut output = Vec::new();
    for event in events {
        if let Some(min_mono_ns) = min_mono_ns
            && event.mono_ns < min_mono_ns
        {
            continue;
        }
        if matches!(event.kind, RecordingEventKind::PaneOutputRaw)
            && let RecordingPayload::Bytes { data } = &event.payload
        {
            output.extend_from_slice(data);
        }
    }
    output
}

pub(super) fn input_timeline(events: &[RecordingEventEnvelope]) -> Vec<ReplayInputEvent> {
    events
        .iter()
        .filter_map(|event| {
            if !matches!(event.kind, RecordingEventKind::PaneInputRaw) {
                return None;
            }
            match &event.payload {
                RecordingPayload::Bytes { data } => Some(ReplayInputEvent {
                    mono_ns: event.mono_ns,
                    data: data.clone(),
                }),
                _ => None,
            }
        })
        .collect()
}

/// Extract viewport dimensions from recording events by finding the first
/// `AttachSetViewport` request. Returns `None` if no viewport was recorded.
pub(super) fn extract_viewport_from_events(
    events: &[RecordingEventEnvelope],
) -> Option<(u16, u16)> {
    for event in events {
        if let (
            RecordingEventKind::RequestStart,
            RecordingPayload::RequestStart { request_data, .. },
        ) = (&event.kind, &event.payload)
        {
            if request_data.is_empty() {
                continue;
            }
            if let Ok(request) = bmux_ipc::decode::<bmux_ipc::Request>(request_data)
                && let bmux_ipc::Request::AttachSetViewport { cols, rows, .. } = request
            {
                return Some((cols, rows));
            }
        }
    }
    None
}

/// Render raw output bytes through a vt100 terminal emulator and return the
/// visible screen text.
pub(super) fn render_output_via_vt100(output: &[u8], cols: u16, rows: u16) -> String {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(output);
    let screen = parser.screen();
    let mut lines = Vec::new();
    for row in 0..rows {
        lines.push(screen.contents_between(row, 0, row, cols));
    }
    // Trim trailing empty lines.
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Normalize screen text for structural comparison: collapse digit sequences
/// to a placeholder, trim trailing whitespace per line.
pub(super) fn normalize_screen_text(text: &str) -> String {
    text.lines()
        .map(|line| {
            let trimmed = line.trim_end();
            // Replace sequences of digits with a placeholder to tolerate PIDs,
            // timestamps, and other non-deterministic numeric values.
            let mut result = String::new();
            let mut chars = trimmed.chars().peekable();
            while let Some(ch) = chars.next() {
                if ch.is_ascii_digit() {
                    while chars.peek().is_some_and(char::is_ascii_digit) {
                        chars.next();
                    }
                    result.push_str("<N>");
                } else {
                    result.push(ch);
                }
            }
            result
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Find the first line where two texts differ and return a human-readable
/// description.
pub(super) fn find_text_mismatch(expected: &str, actual: &str) -> String {
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();
    for (i, (e, a)) in expected_lines.iter().zip(actual_lines.iter()).enumerate() {
        if e != a {
            return format!(
                "line {}: expected {:?}, got {:?}",
                i + 1,
                truncate_str(e, 80),
                truncate_str(a, 80)
            );
        }
    }
    if expected_lines.len() != actual_lines.len() {
        return format!(
            "line count: expected {}, got {}",
            expected_lines.len(),
            actual_lines.len()
        );
    }
    "unknown difference".to_string()
}

pub(super) fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        format!("{}...", &s[..max_len])
    } else {
        s.to_string()
    }
}

#[allow(clippy::too_many_lines)]
pub(super) async fn run_target_verify_capture(
    target_binary: &Path,
    inputs: &[ReplayInputEvent],
    strict_timing: bool,
    max_verify_duration_secs: Option<u64>,
    verify_start_timeout_secs: Option<u64>,
    viewport: Option<(u16, u16)>,
) -> Result<Vec<u8>> {
    let max_verify_duration = max_verify_duration_secs.map(Duration::from_secs);
    let (paths, root_dir) = verify_temp_paths();
    paths
        .ensure_dirs()
        .context("failed preparing verify temp paths")?;
    write_verify_config(&paths)?;

    let verify_start_timeout =
        verify_start_timeout_secs.map_or(VERIFY_SERVER_START_TIMEOUT_DEFAULT, Duration::from_secs);
    let mut server = start_verify_server(target_binary, &paths, &root_dir, verify_start_timeout)
        .await
        .with_context(|| format!("verify startup failed; artifacts at {}", root_dir.display()))?;

    let run_result = async {
        wait_for_verify_server_ready(&paths, Duration::from_secs(5)).await?;
        let mut client = BmuxClient::connect_with_paths(&paths, "bmux-cli-recording-verify")
            .await
            .map_err(map_cli_client_error)?;
        let session_id = client
            .new_session(Some("verify-replay".to_string()))
            .await
            .map_err(map_cli_client_error)?;
        let grant = client
            .attach_grant(SessionSelector::ById(session_id))
            .await
            .map_err(map_cli_client_error)?;
        let attach = client
            .open_attach_stream_info(&grant)
            .await
            .map_err(map_cli_client_error)?;
        let (vp_cols, vp_rows) = viewport.unwrap_or((120, 40));
        let _ = client
            .attach_set_viewport(attach.session_id, vp_cols, vp_rows)
            .await
            .map_err(map_cli_client_error);

        let mut output = Vec::new();
        let mut last_input_ns = 0_u64;
        let verify_started = Instant::now();
        for input in inputs {
            if let Some(limit) = max_verify_duration
                && verify_started.elapsed() > limit
            {
                anyhow::bail!(
                    "verify aborted after exceeding max duration of {}s",
                    limit.as_secs()
                );
            }
            if input.mono_ns > last_input_ns {
                let delta = input.mono_ns.saturating_sub(last_input_ns);
                let sleep_ns = if strict_timing {
                    delta
                } else {
                    delta.min(25_000_000)
                };
                if sleep_ns > 0 {
                    tokio::time::sleep(Duration::from_nanos(sleep_ns)).await;
                }
            }
            if !input.data.is_empty() {
                client
                    .attach_input(attach.session_id, input.data.clone())
                    .await
                    .map_err(map_cli_client_error)?;
            }
            let _ = collect_attach_output_until_idle(
                &mut client,
                attach.session_id,
                &mut output,
                Duration::from_millis(500),
            )
            .await;
            last_input_ns = input.mono_ns;
        }
        for _ in 0..6 {
            if let Some(limit) = max_verify_duration
                && verify_started.elapsed() > limit
            {
                anyhow::bail!(
                    "verify aborted after exceeding max duration of {}s",
                    limit.as_secs()
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let _ = collect_attach_output_until_idle(
            &mut client,
            attach.session_id,
            &mut output,
            Duration::from_millis(600),
        )
        .await;
        Ok::<Vec<u8>, anyhow::Error>(output)
    }
    .await;

    let stop_result = server.shutdown().await;
    if run_result.is_ok() && stop_result.is_ok() {
        let _ = std::fs::remove_dir_all(&root_dir);
    } else {
        warn!(
            "recording verify artifacts retained at {}",
            root_dir.display()
        );
        warn!(
            "recording verify server stdout log: {}",
            server.stdout_log_path().display()
        );
        warn!(
            "recording verify server stderr log: {}",
            server.stderr_log_path().display()
        );
    }

    if let Err(error) = stop_result {
        return Err(error).with_context(|| {
            format!(
                "verify server shutdown failed; artifacts at {} (stdout: {}, stderr: {})",
                root_dir.display(),
                server.stdout_log_path().display(),
                server.stderr_log_path().display()
            )
        });
    }

    if let Err(error) = run_result {
        return Err(error).with_context(|| {
            format!(
                "verify run failed; artifacts at {} (stdout: {}, stderr: {})",
                root_dir.display(),
                server.stdout_log_path().display(),
                server.stderr_log_path().display()
            )
        });
    }

    run_result
}

pub(super) async fn wait_for_verify_server_ready(
    paths: &ConfigPaths,
    timeout: Duration,
) -> Result<()> {
    let start = Instant::now();
    let mut poll_delay = Duration::from_millis(50);
    loop {
        match BmuxClient::connect_with_paths(paths, "bmux-cli-recording-verify-ready").await {
            Ok(_) => return Ok(()),
            Err(_) if start.elapsed() < timeout => {
                tokio::time::sleep(poll_delay).await;
                poll_delay = (poll_delay * 2).min(Duration::from_millis(250));
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "verify server did not become ready: {error}"
                ));
            }
        }
    }
}

pub(super) async fn drain_attach_output(
    client: &mut BmuxClient,
    session_id: Uuid,
    output: &mut Vec<u8>,
) -> Result<usize> {
    let mut total = 0_usize;
    loop {
        let chunk = client
            .attach_output(session_id, 65_536)
            .await
            .map_err(map_cli_client_error)?;
        if chunk.is_empty() {
            break;
        }
        total = total.saturating_add(chunk.len());
        output.extend_from_slice(&chunk);
    }
    Ok(total)
}

pub(super) async fn collect_attach_output_until_idle(
    client: &mut BmuxClient,
    session_id: Uuid,
    output: &mut Vec<u8>,
    max_wait: Duration,
) -> Result<usize> {
    let started = Instant::now();
    let mut collected = 0_usize;
    let mut idle_polls = 0_u8;
    while started.elapsed() < max_wait {
        let read = drain_attach_output(client, session_id, output).await?;
        collected = collected.saturating_add(read);
        if read == 0 {
            idle_polls = idle_polls.saturating_add(1);
            if idle_polls >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        } else {
            idle_polls = 0;
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    Ok(collected)
}

#[derive(Debug)]
pub(super) enum VerifyServerHandle {
    Foreground {
        child: std::process::Child,
        paths: ConfigPaths,
        stdout_log: PathBuf,
        stderr_log: PathBuf,
    },
    Daemon {
        paths: ConfigPaths,
        stdout_log: PathBuf,
        stderr_log: PathBuf,
    },
}

impl VerifyServerHandle {
    async fn shutdown(&mut self) -> Result<()> {
        stop_verify_server(self.paths()).await?;
        match self {
            Self::Foreground { child, .. } => {
                if wait_for_child_exit(child, Duration::from_secs(2)).await? {
                    return Ok(());
                }
                if try_kill_pid(child.id())? {
                    let _ = wait_for_child_exit(child, Duration::from_secs(2)).await;
                }
                Ok(())
            }
            Self::Daemon { paths, .. } => {
                if wait_until_verify_server_stopped(paths, Duration::from_secs(2)).await? {
                    return Ok(());
                }
                if let Some(pid) = read_server_pid_file_at(paths)? {
                    let _ = try_kill_pid(pid);
                }
                Ok(())
            }
        }
    }

    const fn paths(&self) -> &ConfigPaths {
        match self {
            Self::Foreground { paths, .. } | Self::Daemon { paths, .. } => paths,
        }
    }

    fn stdout_log_path(&self) -> &Path {
        match self {
            Self::Foreground { stdout_log, .. } | Self::Daemon { stdout_log, .. } => {
                stdout_log.as_path()
            }
        }
    }

    fn stderr_log_path(&self) -> &Path {
        match self {
            Self::Foreground { stderr_log, .. } | Self::Daemon { stderr_log, .. } => {
                stderr_log.as_path()
            }
        }
    }
}

pub(super) async fn start_verify_server(
    target_binary: &Path,
    paths: &ConfigPaths,
    root_dir: &Path,
    timeout: Duration,
) -> Result<VerifyServerHandle> {
    match start_verify_server_foreground(target_binary, paths, root_dir, timeout).await {
        Ok(handle) => Ok(handle),
        Err(foreground_error) => {
            warn!(
                "recording verify foreground server startup failed, falling back to daemon: {foreground_error}"
            );
            start_verify_server_daemon(target_binary, paths, root_dir, timeout)
                .await
                .with_context(|| {
                    format!(
                        "verify startup failed in foreground and daemon fallback; foreground error: {foreground_error:#}"
                    )
                })
        }
    }
}

pub(super) async fn start_verify_server_foreground(
    target_binary: &Path,
    paths: &ConfigPaths,
    root_dir: &Path,
    timeout: Duration,
) -> Result<VerifyServerHandle> {
    let logs_dir = root_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)
        .with_context(|| format!("failed creating verify logs dir {}", logs_dir.display()))?;
    let stdout_log = logs_dir.join("verify-server-foreground.stdout.log");
    let stderr_log = logs_dir.join("verify-server-foreground.stderr.log");
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stdout_log)
        .with_context(|| format!("failed opening verify stdout log {}", stdout_log.display()))?;
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_log)
        .with_context(|| format!("failed opening verify stderr log {}", stderr_log.display()))?;

    let child = ProcessCommand::new(target_binary)
        .arg("server")
        .arg("start")
        .env("BMUX_CONFIG_DIR", &paths.config_dir)
        .env("BMUX_RUNTIME_DIR", &paths.runtime_dir)
        .env("BMUX_DATA_DIR", &paths.data_dir)
        .env("BMUX_STATE_DIR", &paths.state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| {
            format!(
                "failed spawning foreground verify target binary {}",
                target_binary.display()
            )
        })?;

    let mut handle = VerifyServerHandle::Foreground {
        child,
        paths: paths.clone(),
        stdout_log: stdout_log.clone(),
        stderr_log: stderr_log.clone(),
    };

    match wait_for_verify_server_ready_with_child(paths, timeout, handle.child_mut()).await {
        Ok(()) => Ok(handle),
        Err(error) => {
            let stderr_excerpt = read_verify_log_excerpt(&stderr_log);
            let _ = handle.shutdown().await;
            Err(error).with_context(|| {
                format!(
                    "foreground verify startup failed (stdout: {}, stderr: {}, stderr_excerpt: {})",
                    stdout_log.display(),
                    stderr_log.display(),
                    stderr_excerpt
                )
            })
        }
    }
}

pub(super) async fn start_verify_server_daemon(
    target_binary: &Path,
    paths: &ConfigPaths,
    root_dir: &Path,
    timeout: Duration,
) -> Result<VerifyServerHandle> {
    let logs_dir = root_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)
        .with_context(|| format!("failed creating verify logs dir {}", logs_dir.display()))?;
    let stdout_log = logs_dir.join("verify-server-daemon.stdout.log");
    let stderr_log = logs_dir.join("verify-server-daemon.stderr.log");
    let output = ProcessCommand::new(target_binary)
        .arg("server")
        .arg("start")
        .arg("--daemon")
        .env("BMUX_CONFIG_DIR", &paths.config_dir)
        .env("BMUX_RUNTIME_DIR", &paths.runtime_dir)
        .env("BMUX_DATA_DIR", &paths.data_dir)
        .env("BMUX_STATE_DIR", &paths.state_dir)
        .output()
        .context("failed starting verify target daemon fallback")?;
    std::fs::write(&stdout_log, &output.stdout)
        .with_context(|| format!("failed writing verify stdout log {}", stdout_log.display()))?;
    std::fs::write(&stderr_log, &output.stderr)
        .with_context(|| format!("failed writing verify stderr log {}", stderr_log.display()))?;
    if !output.status.success() {
        let stderr_excerpt = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "verify daemon fallback start failed with status {} (stdout: {}, stderr: {}, stderr_excerpt: {})",
            output.status,
            stdout_log.display(),
            stderr_log.display(),
            stderr_excerpt
        );
    }
    wait_for_verify_server_ready(paths, timeout).await?;
    Ok(VerifyServerHandle::Daemon {
        paths: paths.clone(),
        stdout_log,
        stderr_log,
    })
}

pub(super) async fn wait_for_verify_server_ready_with_child(
    paths: &ConfigPaths,
    timeout: Duration,
    child: Option<&mut std::process::Child>,
) -> Result<()> {
    let start = Instant::now();
    let mut poll_delay = Duration::from_millis(50);
    let mut child = child;
    loop {
        match BmuxClient::connect_with_paths(paths, "bmux-cli-recording-verify-ready").await {
            Ok(_) => return Ok(()),
            Err(_) if start.elapsed() < timeout => {
                if let Some(child) = child.as_deref_mut()
                    && let Some(status) = child
                        .try_wait()
                        .context("failed checking verify target process status")?
                {
                    anyhow::bail!(
                        "verify target process exited before readiness (status: {status})"
                    );
                }
                tokio::time::sleep(poll_delay).await;
                poll_delay = (poll_delay * 2).min(Duration::from_millis(250));
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "verify server did not become ready within {}s: {error}",
                    timeout.as_secs()
                ));
            }
        }
    }
}

pub(super) async fn wait_until_verify_server_stopped(
    paths: &ConfigPaths,
    timeout: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match BmuxClient::connect_with_paths(paths, "bmux-cli-recording-verify-stop-check").await {
            Ok(_) => tokio::time::sleep(Duration::from_millis(80)).await,
            Err(_) => return Ok(true),
        }
    }
    Ok(false)
}

pub(super) async fn wait_for_child_exit(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child
            .try_wait()
            .context("failed checking verify child process state")?
            .is_some()
        {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    Ok(child
        .try_wait()
        .context("failed checking verify child process state")?
        .is_some())
}

pub(super) fn read_server_pid_file_at(paths: &ConfigPaths) -> Result<Option<u32>> {
    let pid_file = paths.server_pid_file();
    let content = match std::fs::read_to_string(&pid_file) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed reading pid file {}", pid_file.display()));
        }
    };
    Ok(parse_pid_content(&content))
}

pub(super) fn read_verify_log_excerpt(path: &Path) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| content.lines().last().map(str::to_string))
        .filter(|line| !line.trim().is_empty())
        .unwrap_or_else(|| "<empty>".to_string())
}

impl VerifyServerHandle {
    const fn child_mut(&mut self) -> Option<&mut std::process::Child> {
        match self {
            Self::Foreground { child, .. } => Some(child),
            Self::Daemon { .. } => None,
        }
    }
}

pub(super) async fn stop_verify_server(paths: &ConfigPaths) -> Result<()> {
    if let Ok(mut client) =
        BmuxClient::connect_with_paths(paths, "bmux-cli-recording-verify-stop").await
    {
        let _ = client.stop_server().await.map_err(map_cli_client_error);
    }
    Ok(())
}

pub(super) fn verify_temp_paths() -> (ConfigPaths, PathBuf) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let root = std::env::temp_dir().join(format!("brv-{nanos:x}"));
    let paths = ConfigPaths::new(
        root.join("c"),
        root.join("r"),
        root.join("d"),
        root.join("s"),
    );
    (paths, root)
}

pub(super) fn write_verify_config(paths: &ConfigPaths) -> Result<()> {
    let config_path = paths.config_file();
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating verify config dir {}", parent.display()))?;
    }
    let config = BmuxConfig::default();
    let registry = scan_available_plugins(&config, paths)?;
    let bundled_roots = bundled_plugin_roots()
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let mut disabled_plugins = registry
        .iter()
        .filter(|&plugin| {
            bundled_roots.contains(&plugin.search_root) && registered_plugin_entry_exists(plugin)
        })
        .map(|plugin| plugin.declaration.id.as_str().to_string())
        .collect::<Vec<_>>();
    disabled_plugins.sort();

    let disabled = if disabled_plugins.is_empty() {
        String::new()
    } else {
        disabled_plugins
            .iter()
            .map(|id| format!("'{id}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let payload = format!("[plugins]\ndisabled = [{disabled}]\n");
    std::fs::write(&config_path, payload)
        .with_context(|| format!("failed writing verify config {}", config_path.display()))
}

pub(super) fn parse_ignore_rules(ignore: Option<&str>) -> Vec<String> {
    recording::parse_ignore_rules(ignore)
}

pub(super) fn apply_ignore_rules(
    events: &[RecordingEventEnvelope],
    ignore_rules: &[String],
) -> Vec<RecordingEventEnvelope> {
    recording::apply_ignore_rules(events, ignore_rules)
}

pub(super) fn recording_event_kind_name(kind: RecordingEventKind) -> String {
    recording::recording_event_kind_name(kind)
}

pub(super) fn load_recording_events(recording_id: &str) -> Result<Vec<RecordingEventEnvelope>> {
    recording::load_recording_events(recording_id)
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;

    fn make_event(
        kind: RecordingEventKind,
        mono_ns: u64,
        payload: RecordingPayload,
    ) -> RecordingEventEnvelope {
        RecordingEventEnvelope {
            seq: mono_ns,
            mono_ns,
            wall_epoch_ms: 0,
            session_id: None,
            pane_id: None,
            client_id: None,
            kind,
            payload,
        }
    }

    #[test]
    #[allow(clippy::float_cmp)] // Test assertions with exact expected values
    fn replay_speed_normalization_clamps_invalid_values() {
        assert_eq!(normalize_replay_speed(0.0), 1.0);
        assert_eq!(normalize_replay_speed(-4.0), 1.0);
        assert_eq!(normalize_replay_speed(f64::NAN), 1.0);
        assert_eq!(
            normalize_replay_speed(REPLAY_SPEED_MIN / 8.0),
            REPLAY_SPEED_MIN
        );
        assert_eq!(
            normalize_replay_speed(REPLAY_SPEED_MAX * 4.0),
            REPLAY_SPEED_MAX
        );
    }

    #[test]
    fn replay_controls_map_expected_keys() {
        assert_eq!(
            replay_action_from_key(KeyCode::Char(' '), KeyModifiers::NONE, KeyEventKind::Press),
            Some(InteractiveReplayAction::TogglePause)
        );
        assert_eq!(
            replay_action_from_key(KeyCode::Char('.'), KeyModifiers::NONE, KeyEventKind::Press),
            Some(InteractiveReplayAction::Step)
        );
        assert_eq!(
            replay_action_from_key(KeyCode::Char('!'), KeyModifiers::NONE, KeyEventKind::Press),
            Some(InteractiveReplayAction::OpenShell)
        );
        assert_eq!(
            replay_action_from_key(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
                KeyEventKind::Press
            ),
            Some(InteractiveReplayAction::Quit)
        );
        assert_eq!(
            replay_action_from_key(KeyCode::Char('x'), KeyModifiers::NONE, KeyEventKind::Press),
            None
        );
        assert_eq!(
            replay_action_from_key(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                KeyEventKind::Release
            ),
            None
        );
    }

    #[test]
    fn replay_timeline_step_consumes_until_visible_output() {
        let events = vec![
            make_event(
                RecordingEventKind::PaneInputRaw,
                5,
                RecordingPayload::Bytes { data: vec![b'i'] },
            ),
            make_event(
                RecordingEventKind::PaneOutputRaw,
                10,
                RecordingPayload::Bytes {
                    data: b"hello".to_vec(),
                },
            ),
        ];
        let mut timeline = ReplayTimeline::new(&events);
        let mut out = Vec::new();

        let wrote_output = timeline
            .step_to_next_output(&mut out)
            .expect("step should succeed");

        assert!(wrote_output);
        assert_eq!(out, b"hello");
        assert_eq!(timeline.last_ns, 10);
        assert_eq!(timeline.next_index, 2);
        assert!(timeline.is_finished());
    }

    #[test]
    fn replay_timeline_starts_without_initial_delay() {
        let events = vec![make_event(
            RecordingEventKind::PaneOutputRaw,
            5_000_000,
            RecordingPayload::Bytes {
                data: b"x".to_vec(),
            },
        )];
        let timeline = ReplayTimeline::new(&events);
        assert_eq!(timeline.next_delay(1.0), Duration::ZERO);
    }
}
