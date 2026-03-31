use super::{
    AttachDisplayCapturePlan, BmuxConfig, BufWriter, ConfigPaths, ConnectionPolicyScope, Context,
    GifEncoder, GifFrame, Instant, IsTerminal, Path, PathBuf, RecordingCursorBlinkMode,
    RecordingCursorMode, RecordingCursorShape, RecordingEventEnvelope, RecordingEventKind,
    RecordingEventKindArg, RecordingExportFormat, RecordingProfileArg, RecordingRenderMode,
    RecordingReplayMode, RecordingStatus, RecordingSummary, Repeat, Result, Uuid, Write,
    cleanup_stale_pid_file, connect_if_running, io, map_cli_client_error, parse_uuid_value,
    terminal,
};
use ab_glyph::{Font, FontArc, FontVec, PxScale, ScaleFont, point};
use bmux_fonts::FontPreset;
use font8x8::UnicodeFonts;
use resvg::{tiny_skia, usvg};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

mod terminal_profile;

pub(super) async fn run_recording_start(
    session_id: Option<&str>,
    capture_input: bool,
    profile: Option<RecordingProfileArg>,
    event_kinds: &[RecordingEventKindArg],
) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let mut client = connect_if_running(ConnectionPolicyScope::Normal, "bmux-cli-recording-start")
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "recording start requires a running bmux server.\nRun `bmux server start --daemon` and retry."
            )
        })?;
    let session_id = match session_id {
        Some(raw) => Some(Uuid::parse_str(raw).context("invalid --session-id UUID")?),
        None => None,
    };
    let profile_overridden = profile.is_some();
    let effective_profile = profile.unwrap_or(RecordingProfileArg::Functional);
    let profile = recording_profile_arg_to_ipc(Some(effective_profile));
    let event_kinds = if profile_overridden || !event_kinds.is_empty() {
        resolve_event_kind_override(Some(effective_profile), event_kinds, capture_input)
    } else {
        Some(default_event_kinds_from_config(capture_input))
    };
    let summary = client
        .recording_start(session_id, capture_input, profile, event_kinds)
        .await
        .map_err(map_cli_client_error)?;
    println!(
        "recording started: {} (capture_input={} profile={:?} kinds={})",
        summary.id,
        summary.capture_input,
        summary.profile,
        summary
            .event_kinds
            .iter()
            .map(|kind| recording_event_kind_name(*kind))
            .collect::<Vec<_>>()
            .join(",")
    );
    Ok(0)
}

pub(super) const fn recording_profile_arg_to_ipc(
    profile: Option<RecordingProfileArg>,
) -> Option<bmux_ipc::RecordingProfile> {
    match profile {
        Some(RecordingProfileArg::Full) => Some(bmux_ipc::RecordingProfile::Full),
        Some(RecordingProfileArg::Functional) => Some(bmux_ipc::RecordingProfile::Functional),
        Some(RecordingProfileArg::Visual) => Some(bmux_ipc::RecordingProfile::Visual),
        None => None,
    }
}

pub(super) fn resolve_event_kind_override(
    profile: Option<RecordingProfileArg>,
    event_kinds: &[RecordingEventKindArg],
    capture_input: bool,
) -> Option<Vec<RecordingEventKind>> {
    if !event_kinds.is_empty() {
        return Some(
            event_kinds
                .iter()
                .copied()
                .map(recording_event_kind_arg_to_ipc)
                .collect(),
        );
    }

    let profile = profile?;
    let mut kinds = match profile {
        RecordingProfileArg::Full => vec![
            RecordingEventKind::PaneOutputRaw,
            RecordingEventKind::ProtocolReplyRaw,
            RecordingEventKind::ServerEvent,
            RecordingEventKind::RequestStart,
            RecordingEventKind::RequestDone,
            RecordingEventKind::RequestError,
            RecordingEventKind::Custom,
        ],
        RecordingProfileArg::Functional => vec![
            RecordingEventKind::PaneOutputRaw,
            RecordingEventKind::ServerEvent,
            RecordingEventKind::RequestStart,
            RecordingEventKind::RequestDone,
            RecordingEventKind::RequestError,
            RecordingEventKind::Custom,
        ],
        RecordingProfileArg::Visual => vec![RecordingEventKind::PaneOutputRaw],
    };
    if capture_input && profile != RecordingProfileArg::Visual {
        kinds.push(RecordingEventKind::PaneInputRaw);
    }
    Some(kinds)
}

const fn recording_event_kind_arg_to_ipc(kind: RecordingEventKindArg) -> RecordingEventKind {
    match kind {
        RecordingEventKindArg::PaneInputRaw => RecordingEventKind::PaneInputRaw,
        RecordingEventKindArg::PaneOutputRaw => RecordingEventKind::PaneOutputRaw,
        RecordingEventKindArg::ProtocolReplyRaw => RecordingEventKind::ProtocolReplyRaw,
        RecordingEventKindArg::ServerEvent => RecordingEventKind::ServerEvent,
        RecordingEventKindArg::RequestStart => RecordingEventKind::RequestStart,
        RecordingEventKindArg::RequestDone => RecordingEventKind::RequestDone,
        RecordingEventKindArg::RequestError => RecordingEventKind::RequestError,
        RecordingEventKindArg::Custom => RecordingEventKind::Custom,
    }
}

fn default_event_kinds_from_config(capture_input: bool) -> Vec<RecordingEventKind> {
    let config = BmuxConfig::load().unwrap_or_default();
    let mut kinds = Vec::new();
    if capture_input && config.recording.capture_input {
        kinds.push(RecordingEventKind::PaneInputRaw);
    }
    if config.recording.capture_output {
        kinds.push(RecordingEventKind::PaneOutputRaw);
    }
    if config.recording.capture_events {
        kinds.extend([
            RecordingEventKind::ServerEvent,
            RecordingEventKind::RequestStart,
            RecordingEventKind::RequestDone,
            RecordingEventKind::RequestError,
            RecordingEventKind::Custom,
        ]);
    }
    if kinds.is_empty() {
        kinds.push(RecordingEventKind::PaneOutputRaw);
    }
    kinds
}

pub(super) async fn run_recording_stop(recording_id: Option<&str>) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let mut client = connect_if_running(ConnectionPolicyScope::Normal, "bmux-cli-recording-stop")
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "recording stop requires a running bmux server.\nRun `bmux server start --daemon` and retry."
            )
        })?;
    let recording_id = match recording_id {
        Some(raw) => Some(Uuid::parse_str(raw).context("invalid recording id")?),
        None => None,
    };
    let stopped_id = client
        .recording_stop(recording_id)
        .await
        .map_err(map_cli_client_error)?;
    println!("recording stopped: {stopped_id}");
    Ok(0)
}

pub(super) async fn run_recording_status(as_json: bool) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let status =
        match connect_if_running(ConnectionPolicyScope::Normal, "bmux-cli-recording-status").await?
        {
            Some(mut client) => client
                .recording_status()
                .await
                .map_err(map_cli_client_error)?,
            None => offline_recording_status(),
        };
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status)
                .context("failed encoding recording status json")?
        );
        return Ok(0);
    }
    if let Some(active) = status.active {
        println!(
            "active recording: {} events={} bytes={} capture_input={} profile={:?} kinds={} path={}",
            active.id,
            active.event_count,
            active.payload_bytes,
            active.capture_input,
            active.profile,
            active
                .event_kinds
                .iter()
                .map(|kind| recording_event_kind_name(*kind))
                .collect::<Vec<_>>()
                .join(","),
            active.path
        );
    } else {
        println!("active recording: none");
    }
    Ok(0)
}

pub(super) async fn run_recording_list(as_json: bool) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let recordings =
        match connect_if_running(ConnectionPolicyScope::Normal, "bmux-cli-recording-list").await? {
            Some(mut client) => client
                .recording_list()
                .await
                .map_err(map_cli_client_error)?,
            None => list_recordings_from_disk()?,
        };
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&recordings)
                .context("failed encoding recording list json")?
        );
        return Ok(0);
    }
    for recording in recordings {
        println!(
            "{} started={} ended={} events={} bytes={} capture_input={} profile={:?} kinds={} path={}",
            recording.id,
            recording.started_epoch_ms,
            recording
                .ended_epoch_ms
                .map_or_else(|| "active".to_string(), |value| value.to_string()),
            recording.event_count,
            recording.payload_bytes,
            recording.capture_input,
            recording.profile,
            recording
                .event_kinds
                .iter()
                .map(|kind| recording_event_kind_name(*kind))
                .collect::<Vec<_>>()
                .join(","),
            recording.path
        );
    }
    Ok(0)
}

pub(super) async fn run_recording_delete(recording_id_or_prefix: &str) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    if let Some(mut client) =
        connect_if_running(ConnectionPolicyScope::Normal, "bmux-cli-recording-delete").await?
    {
        let status = client
            .recording_status()
            .await
            .map_err(map_cli_client_error)?;
        let recordings = client
            .recording_list()
            .await
            .map_err(map_cli_client_error)?;
        let resolved = resolve_recording_id_prefix(recording_id_or_prefix, &recordings)?;

        if status
            .active
            .as_ref()
            .is_some_and(|active| active.id == resolved)
        {
            let stopped_id = client
                .recording_stop(Some(resolved))
                .await
                .map_err(map_cli_client_error)?;
            println!("stopped active recording {stopped_id} before delete");
        }

        let deleted_id = client
            .recording_delete(resolved)
            .await
            .map_err(map_cli_client_error)?;
        println!("deleted recording {deleted_id}");
    } else {
        let recordings = list_recordings_from_disk()?;
        let resolved = resolve_recording_id_prefix(recording_id_or_prefix, &recordings)?;
        delete_recording_dir(resolved)?;
        println!("deleted recording {resolved}");
    }
    Ok(0)
}

pub(super) async fn run_recording_delete_all(yes: bool) -> Result<u8> {
    if !confirm_delete_all_recordings(yes)? {
        println!("skipped recording delete-all");
        return Ok(0);
    }

    cleanup_stale_pid_file().await?;
    if let Some(mut client) = connect_if_running(
        ConnectionPolicyScope::Normal,
        "bmux-cli-recording-delete-all",
    )
    .await?
    {
        let status = client
            .recording_status()
            .await
            .map_err(map_cli_client_error)?;
        if let Some(active) = status.active {
            let stopped_id = client
                .recording_stop(Some(active.id))
                .await
                .map_err(map_cli_client_error)?;
            println!("stopped active recording {stopped_id} before delete");
        }
        let deleted_count = client
            .recording_delete_all()
            .await
            .map_err(map_cli_client_error)?;
        println!("deleted {deleted_count} recordings");
    } else {
        let deleted_count = delete_all_recordings_from_disk()?;
        println!("deleted {deleted_count} recordings");
    }
    Ok(0)
}

pub(super) async fn run_recording_prune(older_than: Option<u64>, json: bool) -> Result<u8> {
    cleanup_stale_pid_file().await?;
    let deleted_count = if let Some(mut client) =
        connect_if_running(ConnectionPolicyScope::Normal, "bmux-cli-recording-prune").await?
    {
        client
            .recording_prune(older_than)
            .await
            .map_err(map_cli_client_error)?
    } else {
        let root = recordings_root_dir();
        let config = bmux_config::BmuxConfig::load().unwrap_or_default();
        let retention = older_than.unwrap_or(config.recording.retention_days);
        bmux_server::recording::prune_old_recordings(&root, retention)?
    };

    if json {
        let report = serde_json::json!({
            "deleted_count": deleted_count,
            "older_than_days": older_than,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if deleted_count > 0 {
        println!("pruned {deleted_count} recording(s)");
    } else {
        println!("no recordings to prune");
    }

    Ok(0)
}

pub(super) fn run_recording_inspect(
    recording_id: &str,
    limit: usize,
    kind: Option<&str>,
    as_json: bool,
) -> Result<u8> {
    let events = load_recording_events(recording_id)?;
    let filtered = events
        .into_iter()
        .filter(|event| {
            kind.is_none_or(|kind| {
                recording_event_kind_name(event.kind) == kind.to_ascii_lowercase()
            })
        })
        .take(limit.max(1))
        .collect::<Vec<_>>();
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&filtered)
                .context("failed encoding recording inspect json")?
        );
        return Ok(0);
    }
    for event in filtered {
        println!(
            "seq={} t={} kind={:?} session={:?} pane={:?} client={:?}",
            event.seq, event.mono_ns, event.kind, event.session_id, event.pane_id, event.client_id
        );
    }
    Ok(0)
}

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
    let events = load_recording_events(recording_id)?;
    match mode {
        RecordingReplayMode::Watch => super::replay_watch(&events, speed),
        RecordingReplayMode::Verify => {
            super::replay_verify(
                &events,
                target_bmux,
                compare_recording,
                ignore,
                strict_timing,
                max_verify_duration_secs,
                verify_start_timeout_secs,
            )
            .await
        }
    }
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
    let events = load_recording_events(recording_id)?;
    let report = super::verify_recording_report(
        &events,
        target_bmux,
        compare_recording,
        ignore,
        strict_timing,
        max_verify_duration_secs,
        verify_start_timeout_secs,
    )
    .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report)
            .context("failed encoding verify smoke report json")?
    );
    Ok(u8::from(!report.pass))
}

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
    cursor: RecordingCursorMode,
    cursor_shape: RecordingCursorShape,
    cursor_blink: RecordingCursorBlinkMode,
    cursor_blink_period_ms: u32,
    cursor_color: &str,
    export_metadata: Option<&str>,
    show_progress: bool,
) -> Result<u8> {
    let recording_id = parse_uuid_value(recording_id, "recording id")?;
    let recording_dir = recordings_root_dir().join(recording_id.to_string());
    if !recording_dir.exists() {
        anyhow::bail!("recording not found: {recording_id}")
    }

    let selected_client = if let Some(raw) = view_client {
        parse_uuid_value(raw, "view client id")?
    } else {
        read_recording_owner_client(&recording_dir)?.ok_or_else(|| {
            anyhow::anyhow!("recording missing owner client id; pass --view-client")
        })?
    };

    let events = load_display_track_events(&recording_dir, selected_client)?;
    if events.is_empty() {
        anyhow::bail!(
            "display track is empty for client {selected_client}; cannot export exact-view media"
        )
    }

    let terminal_profile =
        recording_terminal_profile(&events).or_else(terminal_profile::detect_render_profile);

    match format {
        RecordingExportFormat::Gif => export_recording_gif(
            &events,
            output,
            speed,
            fps,
            max_duration,
            max_frames,
            terminal_profile.as_ref(),
            renderer,
            cell_size,
            cell_width,
            cell_height,
            font_family,
            font_size,
            line_height,
            font_path,
            cursor,
            cursor_shape,
            cursor_blink,
            cursor_blink_period_ms,
            cursor_color,
            export_metadata,
            show_progress,
        )?,
    }

    println!("export complete: format={format:?} view_client={selected_client} output={output}");
    Ok(0)
}

fn read_recording_owner_client(recording_dir: &Path) -> Result<Option<Uuid>> {
    let owner_path = recording_dir.join("owner-client-id.txt");
    let content = match std::fs::read_to_string(&owner_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed reading {}", owner_path.display()));
        }
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(parse_uuid_value(trimmed, "owner client id")?))
}

fn load_display_track_events(
    recording_dir: &Path,
    client_id: Uuid,
) -> Result<Vec<DisplayTrackEnvelope>> {
    let path = display_track_path(recording_dir, client_id);
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed reading display track {}", path.display()))?;
    let result = bmux_ipc::read_frames(&bytes)
        .map_err(|e| anyhow::anyhow!("failed parsing display track {}: {e}", path.display()))?;
    if result.bytes_remaining > 0 {
        tracing::warn!(
            "display track {}: {} trailing bytes could not be parsed (truncated?)",
            path.display(),
            result.bytes_remaining
        );
    }
    Ok(result.frames)
}

fn recording_terminal_profile(
    events: &[DisplayTrackEnvelope],
) -> Option<terminal_profile::DetectedTerminalProfile> {
    for envelope in events {
        if let DisplayTrackEvent::StreamOpened {
            terminal_profile: Some(profile_bytes),
            ..
        } = &envelope.event
        {
            if let Ok(profile) =
                bmux_ipc::decode::<terminal_profile::DetectedTerminalProfile>(profile_bytes)
            {
                return Some(profile);
            }
        }
    }
    None
}

#[derive(Clone, Copy, Debug)]
struct CellMetrics {
    width: u16,
    height: u16,
}

fn resolve_export_cell_metrics(
    events: &[DisplayTrackEnvelope],
    cell_size: Option<(u16, u16)>,
    cell_width: Option<u16>,
    cell_height: Option<u16>,
) -> Result<CellMetrics> {
    if cell_size.is_some_and(|(w, h)| w == 0 || h == 0) {
        anyhow::bail!("--cell-size values must be greater than zero")
    }
    if cell_width.is_some_and(|value| value == 0) {
        anyhow::bail!("--cell-width must be greater than zero")
    }
    if cell_height.is_some_and(|value| value == 0) {
        anyhow::bail!("--cell-height must be greater than zero")
    }

    let (size_width, size_height) = cell_size.unwrap_or((0, 0));
    let cli_width = cell_width.or((size_width > 0).then_some(size_width));
    let cli_height = cell_height.or((size_height > 0).then_some(size_height));

    let recorded = recording_cell_metrics(events);
    let current = current_terminal_cell_metrics();
    let width = cli_width
        .or(recorded.map(|value| value.width))
        .or(current.map(|value| value.width))
        .unwrap_or(8);
    let height = cli_height
        .or(recorded.map(|value| value.height))
        .or(current.map(|value| value.height))
        .unwrap_or(16);
    Ok(CellMetrics { width, height })
}

fn recording_cell_metrics(events: &[DisplayTrackEnvelope]) -> Option<CellMetrics> {
    let mut stream_opened = None::<(Option<u16>, Option<u16>, Option<u16>, Option<u16>)>;
    let mut fallback_cols_rows = None::<(u16, u16)>;
    for envelope in events {
        match envelope.event {
            DisplayTrackEvent::StreamOpened {
                cell_width_px,
                cell_height_px,
                window_width_px,
                window_height_px,
                ..
            } => {
                stream_opened = Some((
                    cell_width_px,
                    cell_height_px,
                    window_width_px,
                    window_height_px,
                ));
                if let (Some(width), Some(height)) = (cell_width_px, cell_height_px)
                    && width > 0
                    && height > 0
                {
                    return Some(CellMetrics { width, height });
                }
            }
            DisplayTrackEvent::Resize { cols, rows } => {
                if fallback_cols_rows.is_none() && cols > 0 && rows > 0 {
                    fallback_cols_rows = Some((cols, rows));
                }
            }
            DisplayTrackEvent::FrameBytes { .. } | DisplayTrackEvent::StreamClosed => {}
        }
    }

    let (cell_width_px, cell_height_px, window_width_px, window_height_px) = stream_opened?;
    if let (Some(width), Some(height)) = (cell_width_px, cell_height_px)
        && width > 0
        && height > 0
    {
        return Some(CellMetrics { width, height });
    }
    let (window_width, window_height) = (window_width_px?, window_height_px?);
    let (cols, rows) = fallback_cols_rows?;
    infer_cell_metrics(window_width, window_height, cols, rows)
}

fn current_terminal_cell_metrics() -> Option<CellMetrics> {
    let (cols, rows) = terminal::size().ok()?;
    if cols == 0 || rows == 0 {
        return None;
    }
    let size = terminal::window_size().ok()?;
    infer_cell_metrics(size.width, size.height, cols, rows)
}

fn infer_cell_metrics(
    window_width: u16,
    window_height: u16,
    cols: u16,
    rows: u16,
) -> Option<CellMetrics> {
    if window_width == 0 || window_height == 0 || cols == 0 || rows == 0 {
        return None;
    }
    let width = (window_width / cols).max(1);
    let height = (window_height / rows).max(1);
    Some(CellMetrics { width, height })
}

fn capture_stream_open_metrics() -> (Option<u16>, Option<u16>, Option<u16>, Option<u16>) {
    let (window_width_px, window_height_px) =
        terminal::window_size().ok().map_or((None, None), |value| {
            (
                (value.width > 0).then_some(value.width),
                (value.height > 0).then_some(value.height),
            )
        });

    let (cell_width_px, cell_height_px) = terminal::size()
        .ok()
        .and_then(|(cols, rows)| {
            let window_width = window_width_px?;
            let window_height = window_height_px?;
            infer_cell_metrics(window_width, window_height, cols, rows)
        })
        .map_or((None, None), |value| {
            (Some(value.width), Some(value.height))
        });

    (
        cell_width_px,
        cell_height_px,
        window_width_px,
        window_height_px,
    )
}

#[derive(Debug, Clone, Copy)]
enum CursorVisualShape {
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, Copy)]
struct CursorReplayState {
    shape: CursorVisualShape,
    blink_enabled: bool,
}

impl Default for CursorReplayState {
    fn default() -> Self {
        Self {
            shape: CursorVisualShape::Block,
            blink_enabled: true,
        }
    }
}

#[derive(Debug, Clone)]
struct CursorExportOptions {
    mode: RecordingCursorMode,
    shape: RecordingCursorShape,
    blink: RecordingCursorBlinkMode,
    blink_period_ns: u64,
    color_override: Option<(u8, u8, u8)>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ExportCursorFrame {
    mono_ns: u64,
    row: u16,
    col: u16,
    visible: bool,
    shape: &'static str,
    blink_on: bool,
}

#[derive(Debug, serde::Serialize)]
struct ExportMetadata<'a> {
    format: &'a str,
    output: &'a str,
    fps: u32,
    speed: f64,
    emitted_frames: u32,
    cursor: CursorMetadata<'a>,
    frames: Vec<ExportCursorFrame>,
}

#[derive(Debug, serde::Serialize)]
struct CursorMetadata<'a> {
    mode: &'a str,
    shape: &'a str,
    blink: &'a str,
    blink_period_ms: u32,
    color: &'a str,
}

fn parse_cursor_color(value: &str) -> Result<Option<(u8, u8, u8)>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    let hex = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if hex.len() != 6 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        anyhow::bail!("invalid cursor color '{value}'; expected auto or #RRGGBB")
    }
    let r = u8::from_str_radix(&hex[0..2], 16).context("invalid cursor color red channel")?;
    let g = u8::from_str_radix(&hex[2..4], 16).context("invalid cursor color green channel")?;
    let b = u8::from_str_radix(&hex[4..6], 16).context("invalid cursor color blue channel")?;
    Ok(Some((r, g, b)))
}

fn update_cursor_replay_state(state: &mut CursorReplayState, data: &[u8]) {
    let mut index = 0usize;
    while index + 4 < data.len() {
        if data[index] != 0x1b || data[index + 1] != b'[' {
            index += 1;
            continue;
        }
        let mut cursor = index + 2;
        let mut value: u16 = 0;
        let mut saw_digit = false;
        while cursor < data.len() && data[cursor].is_ascii_digit() {
            saw_digit = true;
            value = value
                .saturating_mul(10)
                .saturating_add(u16::from(data[cursor].saturating_sub(b'0')));
            cursor += 1;
        }
        if cursor + 1 >= data.len() || data[cursor] != b' ' || data[cursor + 1] != b'q' {
            index += 1;
            continue;
        }
        let ps = if saw_digit { value } else { 0 };
        match ps {
            0 | 1 => {
                state.shape = CursorVisualShape::Block;
                state.blink_enabled = true;
            }
            2 => {
                state.shape = CursorVisualShape::Block;
                state.blink_enabled = false;
            }
            3 => {
                state.shape = CursorVisualShape::Underline;
                state.blink_enabled = true;
            }
            4 => {
                state.shape = CursorVisualShape::Underline;
                state.blink_enabled = false;
            }
            5 => {
                state.shape = CursorVisualShape::Bar;
                state.blink_enabled = true;
            }
            6 => {
                state.shape = CursorVisualShape::Bar;
                state.blink_enabled = false;
            }
            _ => {}
        }
        index = cursor + 2;
    }
}

fn effective_cursor_shape(
    options: &CursorExportOptions,
    replay_state: CursorReplayState,
) -> CursorVisualShape {
    match options.shape {
        RecordingCursorShape::Auto => replay_state.shape,
        RecordingCursorShape::Block => CursorVisualShape::Block,
        RecordingCursorShape::Bar => CursorVisualShape::Bar,
        RecordingCursorShape::Underline => CursorVisualShape::Underline,
    }
}

fn compute_cursor_visibility(
    options: &CursorExportOptions,
    replay_state: CursorReplayState,
    parser_visible: bool,
    mono_ns: u64,
    blink_anchor_ns: &mut Option<u64>,
) -> (bool, bool) {
    let base_visible = match options.mode {
        RecordingCursorMode::Auto => parser_visible,
        RecordingCursorMode::On => true,
        RecordingCursorMode::Off => false,
    };
    if !base_visible {
        return (false, true);
    }
    let blink_enabled = match options.blink {
        RecordingCursorBlinkMode::Auto => replay_state.blink_enabled,
        RecordingCursorBlinkMode::On => true,
        RecordingCursorBlinkMode::Off => false,
    };
    if !blink_enabled {
        return (true, true);
    }
    let period = options.blink_period_ns.max(1);
    let anchor = *blink_anchor_ns.get_or_insert(mono_ns);
    let phase_ns = mono_ns.saturating_sub(anchor);
    let blink_on = ((phase_ns / period) % 2) == 0;
    (blink_on, blink_on)
}

fn cursor_shape_name(shape: CursorVisualShape) -> &'static str {
    match shape {
        CursorVisualShape::Block => "block",
        CursorVisualShape::Bar => "bar",
        CursorVisualShape::Underline => "underline",
    }
}

fn overlay_cursor_rgba(
    pixels: &mut [u8],
    frame_width: usize,
    frame_height: usize,
    cell_w: usize,
    cell_h: usize,
    row: u16,
    col: u16,
    shape: CursorVisualShape,
    color: (u8, u8, u8),
) {
    if frame_width == 0 || frame_height == 0 || cell_w == 0 || cell_h == 0 {
        return;
    }
    let x0 = usize::from(col).saturating_mul(cell_w);
    let y0 = usize::from(row).saturating_mul(cell_h);
    if x0 >= frame_width || y0 >= frame_height {
        return;
    }

    match shape {
        CursorVisualShape::Block => {
            for py in 0..cell_h {
                let y = y0 + py;
                if y >= frame_height {
                    continue;
                }
                for px in 0..cell_w {
                    let x = x0 + px;
                    if x >= frame_width {
                        continue;
                    }
                    let idx = (y * frame_width + x) * 4;
                    pixels[idx] = 255_u8.saturating_sub(pixels[idx]);
                    pixels[idx + 1] = 255_u8.saturating_sub(pixels[idx + 1]);
                    pixels[idx + 2] = 255_u8.saturating_sub(pixels[idx + 2]);
                    pixels[idx + 3] = 255;
                }
            }
        }
        CursorVisualShape::Bar => {
            let bar_width = (cell_w / 6).max(1);
            for py in 0..cell_h {
                let y = y0 + py;
                if y >= frame_height {
                    continue;
                }
                for px in 0..bar_width {
                    let x = x0 + px;
                    if x >= frame_width {
                        continue;
                    }
                    let idx = (y * frame_width + x) * 4;
                    pixels[idx] = color.0;
                    pixels[idx + 1] = color.1;
                    pixels[idx + 2] = color.2;
                    pixels[idx + 3] = 255;
                }
            }
        }
        CursorVisualShape::Underline => {
            let line_height = (cell_h / 8).max(1);
            let start_y = y0 + cell_h.saturating_sub(line_height);
            for py in start_y..(start_y + line_height) {
                if py >= frame_height {
                    continue;
                }
                for px in 0..cell_w {
                    let x = x0 + px;
                    if x >= frame_width {
                        continue;
                    }
                    let idx = (py * frame_width + x) * 4;
                    pixels[idx] = color.0;
                    pixels[idx + 1] = color.1;
                    pixels[idx + 2] = color.2;
                    pixels[idx + 3] = 255;
                }
            }
        }
    }
}

fn export_recording_gif(
    events: &[DisplayTrackEnvelope],
    output: &str,
    speed: f64,
    fps: u32,
    max_duration: Option<u64>,
    max_frames: Option<u32>,
    terminal_profile: Option<&terminal_profile::DetectedTerminalProfile>,
    renderer: RecordingRenderMode,
    cell_size: Option<(u16, u16)>,
    cell_width: Option<u16>,
    cell_height: Option<u16>,
    font_family: Option<&str>,
    font_size: Option<f32>,
    line_height: Option<f32>,
    font_path: &[String],
    cursor_mode: RecordingCursorMode,
    cursor_shape: RecordingCursorShape,
    cursor_blink: RecordingCursorBlinkMode,
    cursor_blink_period_ms: u32,
    cursor_color: &str,
    export_metadata: Option<&str>,
    show_progress: bool,
) -> Result<()> {
    let speed = if speed <= 0.0 { 1.0 } else { speed };
    let fps = fps.max(1);
    let frame_interval_ns = (1_000_000_000_f64 / f64::from(fps)) as u64;
    let estimate = estimate_export_progress(events, speed, fps, max_duration, max_frames);
    let mut progress = ExportProgress::new(show_progress, estimate);
    let cursor_options = CursorExportOptions {
        mode: cursor_mode,
        shape: cursor_shape,
        blink: cursor_blink,
        blink_period_ns: u64::from(cursor_blink_period_ms.max(1)).saturating_mul(1_000_000),
        color_override: parse_cursor_color(cursor_color)?,
    };

    let mut max_cols = 80_u16;
    let mut max_rows = 24_u16;
    for event in events {
        if let DisplayTrackEvent::Resize { cols, rows } = event.event {
            max_cols = max_cols.max(cols.max(1));
            max_rows = max_rows.max(rows.max(1));
        }
    }

    let cell_metrics = resolve_export_cell_metrics(events, cell_size, cell_width, cell_height)?;
    let cell_w = cell_metrics.width;
    let cell_h = cell_metrics.height;
    let width = max_cols.saturating_mul(cell_w).max(8);
    let height = max_rows.saturating_mul(cell_h).max(8);
    let render_options = build_render_options(
        terminal_profile,
        renderer,
        font_family,
        font_size,
        line_height,
        font_path,
    )?;
    let palette = xterm_256_palette();
    let mut glyph_renderer = match render_options.mode {
        RecordingRenderMode::Font => GlyphRenderer::new(cell_w, cell_h, &render_options),
        RecordingRenderMode::Bitmap => None,
    };
    let mut bitmap_cache = BitmapGlyphCache::new(usize::from(cell_w), usize::from(cell_h));

    let output_path = PathBuf::from(output);
    if let Some(parent) = output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed creating export parent directory {}",
                parent.display()
            )
        })?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&output_path)
        .with_context(|| format!("failed opening export output {}", output_path.display()))?;
    let mut encoder =
        GifEncoder::new(file, width, height, &[]).context("failed creating gif encoder")?;
    encoder
        .set_repeat(Repeat::Infinite)
        .context("failed setting gif repeat")?;

    let mut parser = vt100::Parser::new(max_rows, max_cols, 20_000);
    let mut current_cols = max_cols;
    let mut current_rows = max_rows;
    let mut emitted_frames = 0_u32;
    let mut processed_frame_events = 0_u32;
    let mut previous_emit_ns = None::<u64>;
    let mut cursor_state = CursorReplayState::default();
    let mut cursor_frames = export_metadata.map(|_| Vec::<ExportCursorFrame>::new());
    let mut blink_anchor_ns = None::<u64>;
    let start_mono_ns = events.iter().map(|event| event.mono_ns).min().unwrap_or(0);
    let frame_cutoff_ns = max_frames.map(|limit| {
        if limit == 0 {
            0_u64
        } else {
            u64::from(limit.saturating_sub(1)).saturating_mul(frame_interval_ns)
        }
    });
    let mut considered_event_count = 0_usize;
    let mut end_scaled_ns = 0_u64;
    for event in events {
        let rel_mono_ns = event.mono_ns.saturating_sub(start_mono_ns);
        if let Some(limit_secs) = max_duration
            && rel_mono_ns / 1_000_000_000 > limit_secs
        {
            break;
        }
        let scaled_ns = ((rel_mono_ns as f64) / speed) as u64;
        if let Some(cutoff) = frame_cutoff_ns
            && scaled_ns > cutoff
        {
            break;
        }
        considered_event_count = considered_event_count.saturating_add(1);
        end_scaled_ns = scaled_ns;
    }

    let max_timeline_frames = if considered_event_count == 0 {
        0_u32
    } else {
        let base = end_scaled_ns
            .saturating_div(frame_interval_ns.max(1))
            .saturating_add(1);
        base.min(u64::from(u32::MAX)) as u32
    };
    let target_frames =
        max_frames.map_or(max_timeline_frames, |limit| limit.min(max_timeline_frames));

    let mut event_index = 0_usize;
    for frame_idx in 0..target_frames {
        let frame_time_ns = u64::from(frame_idx).saturating_mul(frame_interval_ns);
        while event_index < considered_event_count {
            let event = &events[event_index];
            let rel_mono_ns = event.mono_ns.saturating_sub(start_mono_ns);
            let scaled_ns = ((rel_mono_ns as f64) / speed) as u64;
            if scaled_ns > frame_time_ns {
                break;
            }
            match &event.event {
                DisplayTrackEvent::Resize { cols, rows } => {
                    current_cols = (*cols).max(1);
                    current_rows = (*rows).max(1);
                    parser.screen_mut().set_size(current_rows, current_cols);
                }
                DisplayTrackEvent::FrameBytes { data } => {
                    update_cursor_replay_state(&mut cursor_state, data);
                    parser.process(data);
                    processed_frame_events = processed_frame_events.saturating_add(1);
                }
                DisplayTrackEvent::StreamOpened { .. } | DisplayTrackEvent::StreamClosed => {}
            }
            event_index = event_index.saturating_add(1);
        }

        if processed_frame_events == 0 {
            progress.update(processed_frame_events, emitted_frames, false);
            continue;
        }

        let delay_cs = previous_emit_ns.map_or(1_u16, |previous| {
            let delta_ns = frame_time_ns.saturating_sub(previous);
            ((delta_ns / 10_000_000).max(1).min(u64::from(u16::MAX))) as u16
        });
        let mut pixels = if render_options.mode == RecordingRenderMode::Font {
            render_screen_rgba_resvg(
                parser.screen(),
                current_rows,
                current_cols,
                max_rows,
                max_cols,
                cell_w,
                cell_h,
                &palette,
                &render_options,
            )
            .unwrap_or_else(|_| {
                render_screen_rgba(
                    parser.screen(),
                    current_rows,
                    current_cols,
                    max_rows,
                    max_cols,
                    cell_w,
                    cell_h,
                    &palette,
                    glyph_renderer.as_mut(),
                    &mut bitmap_cache,
                )
            })
        } else {
            render_screen_rgba(
                parser.screen(),
                current_rows,
                current_cols,
                max_rows,
                max_cols,
                cell_w,
                cell_h,
                &palette,
                glyph_renderer.as_mut(),
                &mut bitmap_cache,
            )
        };
        let (cursor_row, cursor_col) = parser.screen().cursor_position();
        let parser_cursor_visible = !parser.screen().hide_cursor();
        let shape = effective_cursor_shape(&cursor_options, cursor_state);
        let (cursor_visible, blink_on) = compute_cursor_visibility(
            &cursor_options,
            cursor_state,
            parser_cursor_visible,
            frame_time_ns,
            &mut blink_anchor_ns,
        );
        if cursor_visible && cursor_row < current_rows && cursor_col < current_cols {
            let cursor_color_rgb = cursor_options.color_override.unwrap_or_else(|| {
                parser
                    .screen()
                    .cell(cursor_row, cursor_col)
                    .map(|cell| resolved_cell_colors(cell, &palette).0)
                    .unwrap_or((255, 255, 255))
            });
            overlay_cursor_rgba(
                &mut pixels,
                usize::from(width),
                usize::from(height),
                usize::from(cell_w),
                usize::from(cell_h),
                cursor_row,
                cursor_col,
                shape,
                cursor_color_rgb,
            );
        }
        if let Some(frames) = cursor_frames.as_mut() {
            frames.push(ExportCursorFrame {
                mono_ns: frame_time_ns,
                row: cursor_row,
                col: cursor_col,
                visible: cursor_visible,
                shape: cursor_shape_name(shape),
                blink_on,
            });
        }
        let mut frame = GifFrame::from_rgba_speed(width, height, &mut pixels, 1);
        frame.delay = delay_cs;
        encoder
            .write_frame(&frame)
            .context("failed writing gif frame")?;
        previous_emit_ns = Some(frame_time_ns);
        emitted_frames = emitted_frames.saturating_add(1);
        progress.update(processed_frame_events, emitted_frames, false);
    }

    progress.finish(processed_frame_events, emitted_frames);

    if emitted_frames == 0 {
        anyhow::bail!("no drawable frame events found in display track")
    }
    if let Some(path) = export_metadata {
        let metadata_path = PathBuf::from(path);
        if let Some(parent) = metadata_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed creating export metadata parent directory {}",
                    parent.display()
                )
            })?;
        }
        let metadata = ExportMetadata {
            format: "gif",
            output,
            fps,
            speed,
            emitted_frames,
            cursor: CursorMetadata {
                mode: match cursor_options.mode {
                    RecordingCursorMode::Auto => "auto",
                    RecordingCursorMode::On => "on",
                    RecordingCursorMode::Off => "off",
                },
                shape: match cursor_options.shape {
                    RecordingCursorShape::Auto => "auto",
                    RecordingCursorShape::Block => "block",
                    RecordingCursorShape::Bar => "bar",
                    RecordingCursorShape::Underline => "underline",
                },
                blink: match cursor_options.blink {
                    RecordingCursorBlinkMode::Auto => "auto",
                    RecordingCursorBlinkMode::On => "on",
                    RecordingCursorBlinkMode::Off => "off",
                },
                blink_period_ms: cursor_blink_period_ms.max(1),
                color: cursor_color,
            },
            frames: cursor_frames.unwrap_or_default(),
        };
        let json = serde_json::to_vec_pretty(&metadata)
            .context("failed serializing export cursor metadata")?;
        std::fs::write(&metadata_path, json).with_context(|| {
            format!("failed writing export metadata {}", metadata_path.display())
        })?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ExportProgressEstimate {
    total_frame_events: u32,
    estimated_emitted_frames: u32,
}

fn estimate_export_progress(
    events: &[DisplayTrackEnvelope],
    speed: f64,
    fps: u32,
    max_duration: Option<u64>,
    max_frames: Option<u32>,
) -> ExportProgressEstimate {
    let speed = if speed <= 0.0 { 1.0 } else { speed };
    let frame_interval_ns = (1_000_000_000_f64 / f64::from(fps.max(1))) as u64;
    let mut total_frame_events = 0_u32;
    let mut considered_event_count = 0_u32;
    let start_mono_ns = events.iter().map(|event| event.mono_ns).min().unwrap_or(0);
    let frame_cutoff_ns = max_frames.map(|limit| {
        if limit == 0 {
            0_u64
        } else {
            u64::from(limit.saturating_sub(1)).saturating_mul(frame_interval_ns)
        }
    });
    let mut end_scaled_ns = 0_u64;

    for event in events {
        let rel_mono_ns = event.mono_ns.saturating_sub(start_mono_ns);
        if let Some(limit_secs) = max_duration
            && rel_mono_ns / 1_000_000_000 > limit_secs
        {
            break;
        }
        let scaled_ns = ((rel_mono_ns as f64) / speed) as u64;
        if let Some(cutoff) = frame_cutoff_ns
            && scaled_ns > cutoff
        {
            break;
        }
        considered_event_count = considered_event_count.saturating_add(1);
        end_scaled_ns = scaled_ns;
        if let DisplayTrackEvent::FrameBytes { .. } = event.event {
            total_frame_events = total_frame_events.saturating_add(1);
        }
    }

    let base_emitted_frames = if considered_event_count == 0 || total_frame_events == 0 {
        0_u32
    } else {
        end_scaled_ns
            .saturating_div(frame_interval_ns.max(1))
            .saturating_add(1)
            .min(u64::from(u32::MAX)) as u32
    };
    let estimated_emitted_frames =
        max_frames.map_or(base_emitted_frames, |limit| limit.min(base_emitted_frames));

    ExportProgressEstimate {
        total_frame_events,
        estimated_emitted_frames,
    }
}

struct ExportProgress {
    enabled: bool,
    tty: bool,
    started_at: Instant,
    last_update_at: Instant,
    last_line_len: usize,
    last_non_tty_bucket: Option<u32>,
    estimate: ExportProgressEstimate,
}

impl ExportProgress {
    fn new(show_progress: bool, estimate: ExportProgressEstimate) -> Self {
        Self {
            enabled: show_progress,
            tty: show_progress && io::stderr().is_terminal(),
            started_at: Instant::now(),
            last_update_at: Instant::now(),
            last_line_len: 0,
            last_non_tty_bucket: None,
            estimate,
        }
    }

    fn update(&mut self, processed_frame_events: u32, emitted_frames: u32, force: bool) {
        if !self.enabled || self.estimate.total_frame_events == 0 {
            return;
        }

        let now = Instant::now();
        if !force && now.duration_since(self.last_update_at) < std::time::Duration::from_millis(300)
        {
            return;
        }

        let percent = (f64::from(processed_frame_events)
            / f64::from(self.estimate.total_frame_events.max(1))
            * 100.0)
            .clamp(0.0, 100.0);
        let elapsed = now.duration_since(self.started_at);
        let eta = estimate_eta(
            elapsed,
            processed_frame_events,
            self.estimate.total_frame_events,
        );
        let estimated_emitted = self.estimate.estimated_emitted_frames.max(emitted_frames);
        let line = format!(
            "export {percent:5.1}% events {processed_frame_events}/{} frames {emitted_frames}/{} elapsed {} eta {}",
            self.estimate.total_frame_events,
            estimated_emitted,
            format_duration_compact(elapsed),
            eta.map_or_else(|| "--:--".to_string(), format_duration_compact),
        );

        if self.tty {
            let mut padded = line;
            if self.last_line_len > padded.len() {
                padded.push_str(&" ".repeat(self.last_line_len - padded.len()));
            }
            eprint!("\r{padded}");
            let _ = io::stderr().flush();
            self.last_line_len = padded.len();
            self.last_update_at = now;
            return;
        }

        let bucket = percent.floor() as u32 / 10;
        if force
            || self
                .last_non_tty_bucket
                .is_none_or(|previous| bucket > previous)
        {
            eprintln!("{line}");
            self.last_non_tty_bucket = Some(bucket);
            self.last_update_at = now;
        }
    }

    fn finish(&mut self, processed_frame_events: u32, emitted_frames: u32) {
        self.update(processed_frame_events, emitted_frames, true);
        if self.enabled && self.tty {
            eprintln!();
        }
    }
}

fn estimate_eta(
    elapsed: std::time::Duration,
    completed: u32,
    total: u32,
) -> Option<std::time::Duration> {
    if completed == 0 || completed >= total {
        return (completed >= total).then_some(std::time::Duration::from_secs(0));
    }
    let remaining_ratio = f64::from(total.saturating_sub(completed)) / f64::from(completed);
    Some(elapsed.mul_f64(remaining_ratio))
}

fn format_duration_compact(duration: std::time::Duration) -> String {
    let total_secs = duration.as_secs();
    let secs = total_secs % 60;
    let mins = (total_secs / 60) % 60;
    let hours = total_secs / 3600;
    if hours > 0 {
        format!("{hours}:{mins:02}:{secs:02}")
    } else {
        format!("{mins:02}:{secs:02}")
    }
}

fn render_screen_rgba(
    screen: &vt100::Screen,
    rows: u16,
    cols: u16,
    max_rows: u16,
    max_cols: u16,
    cell_w: u16,
    cell_h: u16,
    palette: &[(u8, u8, u8)],
    mut glyph_renderer: Option<&mut GlyphRenderer>,
    bitmap_cache: &mut BitmapGlyphCache,
) -> Vec<u8> {
    let width = usize::from(max_cols.saturating_mul(cell_w));
    let height = usize::from(max_rows.saturating_mul(cell_h));
    let mut pixels = vec![0_u8; width.saturating_mul(height).saturating_mul(4)];
    let cw = usize::from(cell_w);
    let cell_h_usize = usize::from(cell_h);

    for row in 0..rows {
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            let mut fg = vt100_color_to_palette_index(cell.fgcolor(), true);
            let mut bg = vt100_color_to_palette_index(cell.bgcolor(), false);
            if cell.inverse() {
                std::mem::swap(&mut fg, &mut bg);
            }
            let (fg_r, fg_g, fg_b) = palette[usize::from(fg)];
            let (bg_r, bg_g, bg_b) = palette[usize::from(bg)];
            let x0 = usize::from(col).saturating_mul(cw);
            let y0 = usize::from(row).saturating_mul(cell_h_usize);
            for py in 0..cell_h_usize {
                let y = y0 + py;
                if y >= height {
                    continue;
                }
                let row_start = y.saturating_mul(width);
                for px in 0..cw {
                    let x = x0 + px;
                    if x >= width {
                        continue;
                    }
                    let idx = (row_start + x).saturating_mul(4);
                    pixels[idx] = bg_r;
                    pixels[idx + 1] = bg_g;
                    pixels[idx + 2] = bg_b;
                    pixels[idx + 3] = 255;
                }
            }

            let glyph_char = if cell.has_contents() {
                cell.contents().chars().next().unwrap_or(' ')
            } else {
                ' '
            };
            if glyph_char == ' ' {
                continue;
            }

            let mut drawn_with_font = false;
            if let Some(renderer) = glyph_renderer.as_deref_mut() {
                drawn_with_font = renderer.draw_cell(
                    &mut pixels,
                    width,
                    height,
                    x0,
                    y0,
                    glyph_char,
                    (fg_r, fg_g, fg_b),
                    (bg_r, bg_g, bg_b),
                );
            }
            if !drawn_with_font {
                draw_bitmap_glyph_rgba(
                    &mut pixels,
                    width,
                    height,
                    x0,
                    y0,
                    cw,
                    cell_h_usize,
                    glyph_char,
                    (fg_r, fg_g, fg_b),
                    bitmap_cache,
                );
            }
        }
    }

    pixels
}

fn render_screen_rgba_resvg(
    screen: &vt100::Screen,
    rows: u16,
    cols: u16,
    max_rows: u16,
    max_cols: u16,
    cell_w: u16,
    cell_h: u16,
    palette: &[(u8, u8, u8)],
    options: &RenderOptions,
) -> Result<Vec<u8>> {
    let width = usize::from(max_cols.saturating_mul(cell_w));
    let height = usize::from(max_rows.saturating_mul(cell_h));
    let width_u32 = u32::try_from(width).context("render width exceeds u32")?;
    let height_u32 = u32::try_from(height).context("render height exceeds u32")?;
    let cell_w_usize = usize::from(cell_w);
    let cell_h_usize = usize::from(cell_h);

    let preset = font_preset_for_options(options);
    let mut families = if options.font_families.is_empty() {
        bmux_fonts::default_families_for_preset(preset)
    } else {
        options.font_families.clone()
    };
    if families.is_empty() {
        families.push("monospace".to_string());
    }
    let metrics = compute_font_grid_metrics(cell_w, cell_h, options);
    let font_size = options
        .font_size_px
        .or_else(|| metrics.as_ref().map(|value| value.font_size_px))
        .unwrap_or((f32::from(cell_h) * 0.9).max(8.0));
    let top_to_baseline = metrics
        .as_ref()
        .map_or(f32::from(cell_h) * 0.8, |value| value.top_to_baseline_px);
    let font_family_attr = svg_font_family_list(&families);

    let mut svg = String::with_capacity(width.saturating_mul(height / 4).max(1024));
    write!(
        &mut svg,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\">"
    )
    .expect("svg write cannot fail");
    write!(
        &mut svg,
        "<g font-family=\"{}\" font-size=\"{:.3}\" text-rendering=\"optimizeLegibility\" dominant-baseline=\"alphabetic\" font-kerning=\"none\" font-variant-ligatures=\"none\">",
        xml_escape_attr(&font_family_attr),
        font_size
    )
    .expect("svg write cannot fail");

    for row in 0..rows {
        let mut row_runs = Vec::<TextRun>::new();
        let mut current_run = None::<TextRun>;
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            let (mut fg_rgb, bg_rgb) = resolved_cell_colors(cell, palette);
            if cell.dim() {
                fg_rgb = dim_rgb(fg_rgb);
            }
            let bg_rgb =
                composite_with_backdrop(bg_rgb, options.background_opacity, options.backdrop_rgb);
            let x0 = usize::from(col).saturating_mul(cell_w_usize);
            let y0 = usize::from(row).saturating_mul(cell_h_usize);
            write!(
                &mut svg,
                "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"rgb({},{},{})\"/>",
                x0, y0, cell_w_usize, cell_h_usize, bg_rgb.0, bg_rgb.1, bg_rgb.2
            )
            .expect("svg write cannot fail");

            let cell_text = if cell.has_contents() {
                let text = cell.contents();
                if text.is_empty() { " " } else { text }
            } else {
                " "
            };
            let style = TextStyle {
                fg_rgb,
                bold: cell.bold(),
                italic: cell.italic(),
                underline: cell.underline(),
            };
            match current_run.take() {
                Some(mut run) if run.style == style => {
                    run.text.push_str(cell_text);
                    run.cell_count = run.cell_count.saturating_add(1);
                    current_run = Some(run);
                }
                Some(run) => {
                    row_runs.push(run);
                    current_run = Some(TextRun {
                        start_col: col,
                        text: cell_text.to_string(),
                        cell_count: 1,
                        style,
                    });
                }
                None => {
                    current_run = Some(TextRun {
                        start_col: col,
                        text: cell_text.to_string(),
                        cell_count: 1,
                        style,
                    });
                }
            }
        }
        if let Some(run) = current_run.take() {
            row_runs.push(run);
        }
        for run in row_runs {
            let x0 = usize::from(run.start_col).saturating_mul(cell_w_usize);
            let y0 = usize::from(row).saturating_mul(cell_h_usize);
            let text_y = y0 as f32 + top_to_baseline;
            let style_attrs = svg_style_attrs(&run.style);
            let text_length = usize::from(run.cell_count).saturating_mul(cell_w_usize);
            write!(
                &mut svg,
                "<text x=\"{}\" y=\"{:.3}\" fill=\"rgb({},{},{})\" xml:space=\"preserve\" textLength=\"{}\" lengthAdjust=\"spacingAndGlyphs\"{}>{}</text>",
                x0,
                text_y,
                run.style.fg_rgb.0,
                run.style.fg_rgb.1,
                run.style.fg_rgb.2,
                text_length,
                style_attrs,
                xml_escape_text(&run.text)
            )
            .expect("svg write cannot fail");
        }
    }

    svg.push_str("</g></svg>");

    let mut options_usvg = usvg::Options::default();
    options_usvg.font_family = families
        .first()
        .cloned()
        .unwrap_or_else(|| "monospace".to_string());
    options_usvg.font_size = font_size;
    let fontdb = options_usvg.fontdb_mut();
    let _ = bmux_fonts::register_preset_fonts(fontdb, preset);
    fontdb.load_system_fonts();
    for path in &options.font_paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if meta.is_dir() {
            fontdb.load_fonts_dir(path);
        } else if meta.is_file() {
            let _ = fontdb.load_font_file(path);
        }
    }

    let tree = usvg::Tree::from_str(&svg, &options_usvg).context("failed to parse SVG frame")?;
    let mut pixmap = tiny_skia::Pixmap::new(width_u32, height_u32)
        .ok_or_else(|| anyhow::anyhow!("failed to allocate pixmap for SVG frame"))?;
    resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
    Ok(pixmap.take())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextStyle {
    fg_rgb: (u8, u8, u8),
    bold: bool,
    italic: bool,
    underline: bool,
}

#[derive(Debug, Clone)]
struct TextRun {
    start_col: u16,
    text: String,
    cell_count: u16,
    style: TextStyle,
}

fn resolved_cell_colors(
    cell: &vt100::Cell,
    palette: &[(u8, u8, u8)],
) -> ((u8, u8, u8), (u8, u8, u8)) {
    let mut fg = vt100_color_to_palette_index(cell.fgcolor(), true);
    let mut bg = vt100_color_to_palette_index(cell.bgcolor(), false);
    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }
    (palette[usize::from(fg)], palette[usize::from(bg)])
}

fn dim_rgb(rgb: (u8, u8, u8)) -> (u8, u8, u8) {
    (
        (f32::from(rgb.0) * 0.72).round() as u8,
        (f32::from(rgb.1) * 0.72).round() as u8,
        (f32::from(rgb.2) * 0.72).round() as u8,
    )
}

fn composite_with_backdrop(
    rgb: (u8, u8, u8),
    opacity: f32,
    backdrop_rgb: (u8, u8, u8),
) -> (u8, u8, u8) {
    if opacity >= 0.999 {
        return rgb;
    }
    let alpha = opacity.clamp(0.0, 1.0);
    (
        blend_channel(rgb.0, backdrop_rgb.0, alpha),
        blend_channel(rgb.1, backdrop_rgb.1, alpha),
        blend_channel(rgb.2, backdrop_rgb.2, alpha),
    )
}

fn svg_style_attrs(style: &TextStyle) -> String {
    let mut attrs = String::new();
    if style.bold {
        attrs.push_str(" font-weight=\"700\"");
    }
    if style.italic {
        attrs.push_str(" font-style=\"italic\"");
    }
    if style.underline {
        attrs.push_str(" text-decoration=\"underline\"");
    }
    attrs
}

struct FontGridMetrics {
    font_size_px: f32,
    top_to_baseline_px: f32,
}

struct PrimaryFontSource {
    font: FontArc,
    bytes: Vec<u8>,
    face_index: u32,
}

fn compute_font_grid_metrics(
    cell_w: u16,
    cell_h: u16,
    options: &RenderOptions,
) -> Option<FontGridMetrics> {
    let primary = primary_font_source_for_metrics(options)?;
    let unit_scale = PxScale { x: 1.0, y: 1.0 };
    let unit_face_width = ascii_cell_width(&primary.font, unit_scale).max(0.0001);
    let (unit_ascent, unit_descent, unit_line_gap) =
        font_vertical_metrics_px(&primary.bytes, primary.face_index, 1.0).unwrap_or_else(|| {
            let scaled = primary.font.as_scaled(unit_scale);
            (scaled.ascent(), scaled.descent(), scaled.line_gap())
        });
    let unit_face_height = (unit_ascent - unit_descent + unit_line_gap).max(0.0001);
    let target_w = f32::from(cell_w).max(1.0);
    let target_h = f32::from(cell_h).max(1.0);
    let font_size =
        solve_font_size_for_target_cells(unit_face_width, unit_face_height, target_w, target_h)?;

    let (ascent, descent, line_gap) =
        font_vertical_metrics_px(&primary.bytes, primary.face_index, font_size).unwrap_or_else(
            || {
                let scaled = primary.font.as_scaled(PxScale {
                    x: font_size,
                    y: font_size,
                });
                (scaled.ascent(), scaled.descent(), scaled.line_gap())
            },
        );
    let face_height = (ascent - descent + line_gap).max(0.0001);
    let half_line_gap = line_gap / 2.0;
    let face_baseline = half_line_gap - descent;
    let cell_height = target_h;
    let cell_baseline = (face_baseline - (cell_height - face_height) / 2.0).round();
    let top_to_baseline = (cell_height - cell_baseline).max(0.0);

    Some(FontGridMetrics {
        font_size_px: font_size,
        top_to_baseline_px: top_to_baseline,
    })
}

fn font_vertical_metrics_px(
    font_data: &[u8],
    face_index: u32,
    size_px: f32,
) -> Option<(f32, f32, f32)> {
    if !(size_px.is_finite() && size_px > 0.0) {
        return None;
    }
    let face = ttf_parser::Face::parse(font_data, face_index).ok()?;
    let units_per_em = f32::from(face.units_per_em()).max(1.0);
    let px_per_unit = size_px / units_per_em;
    let ascent = f32::from(face.ascender()) * px_per_unit;
    let descent = f32::from(face.descender()) * px_per_unit;
    let line_gap = f32::from(face.line_gap()) * px_per_unit;
    Some((ascent, descent, line_gap))
}

fn ascii_cell_width(font: &FontArc, scale: PxScale) -> f32 {
    let scaled = font.as_scaled(scale);
    let mut max_advance = 0.0_f32;
    for codepoint in 32_u32..127_u32 {
        let Some(ch) = char::from_u32(codepoint) else {
            continue;
        };
        let glyph_id = font.glyph_id(ch);
        if glyph_id.0 == 0 {
            continue;
        }
        max_advance = max_advance.max(scaled.h_advance(glyph_id));
    }
    if max_advance <= 0.0 {
        scaled.h_advance(font.glyph_id('M')).max(0.0001)
    } else {
        max_advance
    }
}

fn solve_font_size_for_target_cells(
    unit_w: f32,
    unit_h: f32,
    target_w: f32,
    target_h: f32,
) -> Option<f32> {
    if !(unit_w.is_finite() && unit_h.is_finite() && unit_w > 0.0 && unit_h > 0.0) {
        return None;
    }

    let h_lo = ((target_h - 0.5) / unit_h).max(0.001);
    let h_hi = (target_h + 0.5) / unit_h;
    if h_lo < h_hi {
        let preferred = target_w / unit_w;
        let size = preferred.clamp(h_lo, h_hi - f32::EPSILON);
        return Some(size.max(0.001));
    }

    let mut candidates = Vec::new();
    candidates.push((target_w / unit_w).max(0.001));
    candidates.push((target_h / unit_h).max(0.001));
    let w_lo = ((target_w - 0.5) / unit_w).max(0.001);
    let w_hi = (target_w + 0.5) / unit_w;
    candidates.push(w_lo);
    candidates.push(w_hi.max(0.001));
    candidates.push(h_lo);
    candidates.push(h_hi.max(0.001));

    let mut best = None::<(f32, f32)>;
    for candidate in candidates {
        if !candidate.is_finite() || candidate <= 0.0 {
            continue;
        }
        let width_err = (unit_w * candidate).round() - target_w;
        let height_err = (unit_h * candidate).round() - target_h;
        let score = height_err.abs().mul_add(2.0, width_err.abs());
        if best.is_none_or(|(_, best_score)| score < best_score) {
            best = Some((candidate, score));
        }
    }

    best.map(|(value, _)| value)
}

fn primary_font_source_for_metrics(options: &RenderOptions) -> Option<PrimaryFontSource> {
    let preset = font_preset_for_options(options);

    let mut db = fontdb::Database::new();
    let _ = bmux_fonts::register_preset_fonts(&mut db, preset);
    db.load_system_fonts();
    for path in &options.font_paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if meta.is_dir() {
            db.load_fonts_dir(path);
        } else if meta.is_file() {
            let _ = db.load_font_file(path);
        }
    }

    let mut families = Vec::<String>::new();
    if !options.font_families.is_empty() {
        families.extend(options.font_families.iter().cloned());
    }
    families.extend(bmux_fonts::default_families_for_preset(preset));
    let mut seen = HashSet::<String>::new();
    for family in families {
        let normalized = family.trim().to_ascii_lowercase();
        if normalized.is_empty() || !seen.insert(normalized) {
            continue;
        }
        if let Some(source) = load_font_family_source_from_db(&db, &family) {
            return Some(source);
        }
    }

    for path in &options.font_paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        if let Ok(bytes) = std::fs::read(path)
            && let Some(source) = primary_font_source_from_bytes(bytes, None)
        {
            return Some(source);
        }
    }

    for embedded in bmux_fonts::bundled_fonts_for_preset(preset) {
        if let Some(source) = primary_font_source_from_bytes(embedded.data.to_vec(), None) {
            return Some(source);
        }
    }

    None
}

fn primary_font_source_from_bytes(
    bytes: Vec<u8>,
    preferred_face_index: Option<u32>,
) -> Option<PrimaryFontSource> {
    if let Some(face_index) = preferred_face_index
        && let Ok(font) = FontVec::try_from_vec_and_index(bytes.clone(), face_index)
    {
        return Some(PrimaryFontSource {
            font: FontArc::new(font),
            bytes,
            face_index,
        });
    }

    let face_count = ttf_parser::fonts_in_collection(&bytes).unwrap_or(1);
    for face_index in 0..face_count {
        let Ok(font) = FontVec::try_from_vec_and_index(bytes.clone(), face_index) else {
            continue;
        };
        return Some(PrimaryFontSource {
            font: FontArc::new(font),
            bytes,
            face_index,
        });
    }

    None
}

fn load_font_family_source_from_db(
    db: &fontdb::Database,
    family: &str,
) -> Option<PrimaryFontSource> {
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight: fontdb::Weight::NORMAL,
        stretch: fontdb::Stretch::Normal,
        style: fontdb::Style::Normal,
    };
    let face_id = db.query(&query)?;
    db.with_face_data(face_id, |font_data, face_index| {
        let bytes = font_data.to_vec();
        let Ok(font) = FontVec::try_from_vec_and_index(bytes.clone(), face_index) else {
            return None;
        };
        Some(PrimaryFontSource {
            font: FontArc::new(font),
            bytes,
            face_index,
        })
    })?
}

fn svg_font_family_list(families: &[String]) -> String {
    families
        .iter()
        .map(|value| format!("'{}'", value.replace('\'', "\\'")))
        .collect::<Vec<_>>()
        .join(", ")
}

fn xml_escape_attr(input: &str) -> String {
    input
        .chars()
        .flat_map(|ch| match ch {
            '&' => "&amp;".chars().collect::<Vec<_>>(),
            '<' => "&lt;".chars().collect::<Vec<_>>(),
            '>' => "&gt;".chars().collect::<Vec<_>>(),
            '\"' => "&quot;".chars().collect::<Vec<_>>(),
            '\'' => "&apos;".chars().collect::<Vec<_>>(),
            _ => vec![ch],
        })
        .collect()
}

fn xml_escape_text(input: &str) -> String {
    input
        .chars()
        .flat_map(|ch| match ch {
            '&' => "&amp;".chars().collect::<Vec<_>>(),
            '<' => "&lt;".chars().collect::<Vec<_>>(),
            '>' => "&gt;".chars().collect::<Vec<_>>(),
            '"' => "&quot;".chars().collect::<Vec<_>>(),
            '\'' => "&apos;".chars().collect::<Vec<_>>(),
            _ => vec![ch],
        })
        .collect()
}

fn draw_bitmap_glyph_rgba(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    x0: usize,
    y0: usize,
    cell_w: usize,
    cell_h: usize,
    glyph_char: char,
    fg_rgb: (u8, u8, u8),
    bitmap_cache: &mut BitmapGlyphCache,
) {
    let Some(mask) = bitmap_cache.mask_for(glyph_char) else {
        return;
    };
    for py in 0..cell_h {
        let y = y0 + py;
        if y >= height {
            continue;
        }
        let row_start = y.saturating_mul(width);
        let mask_row = py.saturating_mul(cell_w);
        for px in 0..cell_w {
            let x = x0 + px;
            if x >= width {
                continue;
            }
            if mask[mask_row + px] == 1 {
                let idx = (row_start + x).saturating_mul(4);
                pixels[idx] = fg_rgb.0;
                pixels[idx + 1] = fg_rgb.1;
                pixels[idx + 2] = fg_rgb.2;
                pixels[idx + 3] = 255;
            }
        }
    }
}

struct BitmapGlyphCache {
    cell_w: usize,
    cell_h: usize,
    masks: HashMap<char, Option<Vec<u8>>>,
}

impl BitmapGlyphCache {
    fn new(cell_w: usize, cell_h: usize) -> Self {
        Self {
            cell_w,
            cell_h,
            masks: HashMap::new(),
        }
    }

    fn mask_for(&mut self, glyph_char: char) -> Option<&[u8]> {
        let entry = self
            .masks
            .entry(glyph_char)
            .or_insert_with(|| build_bitmap_mask(glyph_char, self.cell_w, self.cell_h));
        entry.as_deref()
    }
}

fn build_bitmap_mask(glyph_char: char, cell_w: usize, cell_h: usize) -> Option<Vec<u8>> {
    if cell_w == 0 || cell_h == 0 {
        return None;
    }
    if let Some(mask) = block_element_mask(glyph_char, cell_w, cell_h) {
        return Some(mask);
    }
    let glyph = resolve_bitmap_glyph(glyph_char)?;
    let mut mask = vec![0_u8; cell_w.saturating_mul(cell_h)];
    let mut any_set = false;
    for py in 0..cell_h {
        let glyph_row = ((py.saturating_mul(8)) / cell_h).min(7);
        let bits = glyph[glyph_row];
        let row_start = py.saturating_mul(cell_w);
        for px in 0..cell_w {
            let glyph_col = ((px.saturating_mul(8)) / cell_w).min(7);
            if ((bits >> glyph_col) & 1) == 1 {
                mask[row_start + px] = 1;
                any_set = true;
            }
        }
    }
    any_set.then_some(mask)
}

fn resolve_bitmap_glyph(glyph_char: char) -> Option<[u8; 8]> {
    font8x8::BASIC_FONTS
        .get(glyph_char)
        .or_else(|| font8x8::LATIN_FONTS.get(glyph_char))
        .or_else(|| font8x8::BOX_FONTS.get(glyph_char))
        .or_else(|| font8x8::BLOCK_FONTS.get(glyph_char))
        .or_else(|| font8x8::GREEK_FONTS.get(glyph_char))
        .or_else(|| font8x8::MISC_FONTS.get(glyph_char))
        .or_else(|| font8x8::BASIC_FONTS.get('?'))
}

fn block_element_mask(glyph_char: char, cell_w: usize, cell_h: usize) -> Option<Vec<u8>> {
    let mut mask = vec![0_u8; cell_w.saturating_mul(cell_h)];
    match glyph_char {
        '█' => mask.fill(1),
        '▀' => {
            let cutoff = cell_h.div_ceil(2);
            for y in 0..cutoff {
                let row = y.saturating_mul(cell_w);
                for x in 0..cell_w {
                    mask[row + x] = 1;
                }
            }
        }
        '▄' => {
            let start = cell_h / 2;
            for y in start..cell_h {
                let row = y.saturating_mul(cell_w);
                for x in 0..cell_w {
                    mask[row + x] = 1;
                }
            }
        }
        '▌' => {
            let cutoff = cell_w.div_ceil(2);
            for y in 0..cell_h {
                let row = y.saturating_mul(cell_w);
                for x in 0..cutoff {
                    mask[row + x] = 1;
                }
            }
        }
        '▐' => {
            let start = cell_w / 2;
            for y in 0..cell_h {
                let row = y.saturating_mul(cell_w);
                for x in start..cell_w {
                    mask[row + x] = 1;
                }
            }
        }
        '░' => fill_shade_mask(&mut mask, cell_w, 1),
        '▒' => fill_shade_mask(&mut mask, cell_w, 2),
        '▓' => fill_shade_mask(&mut mask, cell_w, 3),
        _ => return None,
    }
    Some(mask)
}

fn fill_shade_mask(mask: &mut [u8], cell_w: usize, threshold: usize) {
    let threshold = threshold.min(4);
    for (idx, value) in mask.iter_mut().enumerate() {
        let y = idx / cell_w;
        let x = idx % cell_w;
        let matrix_value = (x & 1) + ((y & 1) << 1);
        if matrix_value < threshold {
            *value = 1;
        }
    }
}

struct RenderOptions {
    mode: RecordingRenderMode,
    font_families: Vec<String>,
    font_paths: Vec<String>,
    font_size_px: Option<f32>,
    line_height_mult: f32,
    background_opacity: f32,
    backdrop_rgb: (u8, u8, u8),
}

fn build_render_options(
    terminal_profile: Option<&terminal_profile::DetectedTerminalProfile>,
    renderer: RecordingRenderMode,
    font_family: Option<&str>,
    font_size: Option<f32>,
    line_height: Option<f32>,
    font_path: &[String],
) -> Result<RenderOptions> {
    if font_size.is_some_and(|value| value <= 0.0) {
        anyhow::bail!("--font-size must be greater than zero")
    }
    if line_height.is_some_and(|value| value <= 0.0) {
        anyhow::bail!("--line-height must be greater than zero")
    }
    let font_families = font_family
        .map(parse_csv_values)
        .or_else(|| terminal_profile.map(|profile| profile.font_families.clone()))
        .unwrap_or_default();
    let font_paths = if font_path.is_empty() {
        Vec::new()
    } else {
        font_path.to_vec()
    };
    Ok(RenderOptions {
        mode: renderer,
        font_families,
        font_paths,
        font_size_px: font_size
            .or_else(|| terminal_profile.and_then(|profile| profile.font_size_px.map(f32::from))),
        line_height_mult: line_height.unwrap_or(1.0),
        background_opacity: terminal_profile
            .and_then(|profile| profile.background_opacity_permille)
            .map_or(1.0, |permille| {
                (f32::from(permille) / 1000.0).clamp(0.0, 1.0)
            }),
        backdrop_rgb: (0, 0, 0),
    })
}

fn parse_csv_values(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
}

struct GlyphRenderer {
    fonts: Vec<FontArc>,
    scale: PxScale,
    baseline_offset: f32,
    glyph_font_index: HashMap<char, Option<usize>>,
}

impl GlyphRenderer {
    fn new(cell_w: u16, cell_h: u16, options: &RenderOptions) -> Option<Self> {
        let fonts = load_monospace_fonts(options);
        let font = fonts.first()?;
        let base_font_size = options.font_size_px.unwrap_or(f32::from(cell_h).max(8.0));
        let base_scale = PxScale {
            x: base_font_size,
            y: base_font_size,
        };
        let scaled_base = font.as_scaled(base_scale);
        let measured_advance = scaled_base.h_advance(font.glyph_id('M')).max(0.01);
        let target_advance = (f32::from(cell_w) * 0.92).max(1.0);
        let x_scale = base_scale.x * (target_advance / measured_advance);
        let scale = PxScale {
            x: x_scale,
            y: base_scale.y,
        };
        let scaled = font.as_scaled(scale);
        let text_height = (scaled.ascent() - scaled.descent()).max(1.0);
        let line_height = (text_height * options.line_height_mult.max(1.0)).max(text_height);
        let baseline_offset = ((f32::from(cell_h) - line_height) / 2.0).max(0.0) + scaled.ascent();
        Some(Self {
            fonts,
            scale,
            baseline_offset,
            glyph_font_index: HashMap::new(),
        })
    }

    fn resolve_font_index(&mut self, glyph_char: char) -> Option<usize> {
        if let Some(cached) = self.glyph_font_index.get(&glyph_char) {
            return *cached;
        }
        let resolved = self
            .fonts
            .iter()
            .enumerate()
            .find_map(|(index, font)| (font.glyph_id(glyph_char).0 != 0).then_some(index));
        self.glyph_font_index.insert(glyph_char, resolved);
        resolved
    }

    fn draw_cell(
        &mut self,
        rgba: &mut [u8],
        width: usize,
        height: usize,
        x0: usize,
        y0: usize,
        glyph_char: char,
        fg_rgb: (u8, u8, u8),
        bg_rgb: (u8, u8, u8),
    ) -> bool {
        if glyph_char == ' ' {
            return false;
        }
        let Some(font_index) = self.resolve_font_index(glyph_char) else {
            return false;
        };
        let font = &self.fonts[font_index];
        let glyph = font.glyph_id(glyph_char).with_scale_and_position(
            self.scale,
            point(x0 as f32, y0 as f32 + self.baseline_offset),
        );
        let Some(outlined) = font.outline_glyph(glyph) else {
            return false;
        };
        outlined.draw(|gx, gy, coverage| {
            if coverage <= 0.0 {
                return;
            }
            let x = x0.saturating_add(gx as usize);
            let y = y0.saturating_add(gy as usize);
            if x >= width || y >= height {
                return;
            }
            let alpha = coverage;
            if alpha <= 0.0 {
                return;
            }
            let idx = (y.saturating_mul(width) + x).saturating_mul(4);
            rgba[idx] = blend_channel(fg_rgb.0, bg_rgb.0, alpha);
            rgba[idx + 1] = blend_channel(fg_rgb.1, bg_rgb.1, alpha);
            rgba[idx + 2] = blend_channel(fg_rgb.2, bg_rgb.2, alpha);
            rgba[idx + 3] = 255;
        });
        true
    }
}

fn blend_channel(fg: u8, bg: u8, alpha: f32) -> u8 {
    f32::from(fg)
        .mul_add(alpha, f32::from(bg) * (1.0 - alpha))
        .round() as u8
}

fn load_monospace_fonts(options: &RenderOptions) -> Vec<FontArc> {
    let preset = font_preset_for_options(options);
    let mut fonts = Vec::<FontArc>::new();

    for path in &options.font_paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if let Ok(font) = FontVec::try_from_vec_and_index(bytes, 0) {
            fonts.push(FontArc::new(font));
        }
    }

    fonts.extend(bmux_fonts::load_preset_fonts_for_ab_glyph(preset));

    let mut db = fontdb::Database::new();
    let _ = bmux_fonts::register_preset_fonts(&mut db, preset);
    db.load_system_fonts();
    for path in &options.font_paths {
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if meta.is_dir() {
            db.load_fonts_dir(path);
        } else if meta.is_file() {
            let _ = db.load_font_file(path);
        }
    }

    let mut families = Vec::<String>::new();
    if !options.font_families.is_empty() {
        families.extend(options.font_families.iter().cloned());
    }
    families.extend(bmux_fonts::default_families_for_preset(preset));
    let mut seen = HashSet::<String>::new();
    for family in families {
        let normalized = family.trim().to_ascii_lowercase();
        if normalized.is_empty() || !seen.insert(normalized) {
            continue;
        }
        if let Some(font) = load_font_family_from_db(&db, &family) {
            fonts.push(font);
        }
    }

    fonts
}

fn load_font_family_from_db(db: &fontdb::Database, family: &str) -> Option<FontArc> {
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight: fontdb::Weight::NORMAL,
        stretch: fontdb::Stretch::Normal,
        style: fontdb::Style::Normal,
    };
    let face_id = db.query(&query)?;
    db.with_face_data(face_id, |font_data, face_index| {
        let Ok(font) = FontVec::try_from_vec_and_index(font_data.to_vec(), face_index) else {
            return None;
        };
        Some(FontArc::new(font))
    })?
}

const fn font_preset_for_options(_options: &RenderOptions) -> FontPreset {
    FontPreset::GhosttyNerd
}

fn vt100_color_to_palette_index(color: vt100::Color, foreground: bool) -> u8 {
    match color {
        vt100::Color::Default => {
            if foreground {
                15
            } else {
                0
            }
        }
        vt100::Color::Idx(idx) => idx,
        vt100::Color::Rgb(r, g, b) => nearest_xterm_index(r, g, b),
    }
}

fn nearest_xterm_index(r: u8, g: u8, b: u8) -> u8 {
    let palette = xterm_256_palette();
    let mut best_index = 0_u8;
    let mut best_distance = u32::MAX;
    for (index, (pr, pg, pb)) in palette.iter().enumerate() {
        let dr = i32::from(*pr) - i32::from(r);
        let dg = i32::from(*pg) - i32::from(g);
        let db = i32::from(*pb) - i32::from(b);
        let distance = (dr * dr + dg * dg + db * db) as u32;
        if distance < best_distance {
            best_distance = distance;
            best_index = index as u8;
        }
    }
    best_index
}

fn xterm_256_palette() -> Vec<(u8, u8, u8)> {
    let mut colors = vec![
        (0x00, 0x00, 0x00),
        (0x80, 0x00, 0x00),
        (0x00, 0x80, 0x00),
        (0x80, 0x80, 0x00),
        (0x00, 0x00, 0x80),
        (0x80, 0x00, 0x80),
        (0x00, 0x80, 0x80),
        (0xc0, 0xc0, 0xc0),
        (0x80, 0x80, 0x80),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x00, 0x00, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];

    let steps = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
    for r in steps {
        for g in steps {
            for b in steps {
                colors.push((r, g, b));
            }
        }
    }

    for i in 0..24_u8 {
        let value = 8 + i * 10;
        colors.push((value, value, value));
    }
    colors
}

pub(super) fn parse_ignore_rules(ignore: Option<&str>) -> Vec<String> {
    ignore
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

pub(super) fn apply_ignore_rules(
    events: &[RecordingEventEnvelope],
    ignore_rules: &[String],
) -> Vec<RecordingEventEnvelope> {
    if ignore_rules.is_empty() {
        return events.to_vec();
    }
    events
        .iter()
        .filter(|event| {
            let name = recording_event_kind_name(event.kind);
            !ignore_rules.contains(&name)
        })
        .cloned()
        .collect()
}

pub(super) fn recording_event_kind_name(kind: RecordingEventKind) -> String {
    match kind {
        RecordingEventKind::PaneInputRaw => "pane_input_raw",
        RecordingEventKind::PaneOutputRaw => "pane_output_raw",
        RecordingEventKind::ProtocolReplyRaw => "protocol_reply_raw",
        RecordingEventKind::ServerEvent => "server_event",
        RecordingEventKind::RequestStart => "request_start",
        RecordingEventKind::RequestDone => "request_done",
        RecordingEventKind::RequestError => "request_error",
        RecordingEventKind::Custom => "custom",
    }
    .to_string()
}

pub(super) fn load_recording_events(recording_id: &str) -> Result<Vec<RecordingEventEnvelope>> {
    let id = Uuid::parse_str(recording_id).context("invalid recording id")?;
    let recording_dir = recordings_root_dir().join(id.to_string());
    let manifest_path = recording_dir.join("manifest.json");

    // Read manifest to discover segment files.
    let segments = if manifest_path.exists() {
        let manifest_bytes = std::fs::read(&manifest_path)
            .with_context(|| format!("failed reading manifest {}", manifest_path.display()))?;
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
        manifest["summary"]["segments"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec!["events_0.bin".to_string()])
    } else {
        // Fallback: try legacy single-file format.
        vec!["events.bin".to_string()]
    };

    let mut all_frames = Vec::new();
    for segment_name in &segments {
        let segment_path = recording_dir.join(segment_name);
        if !segment_path.exists() {
            tracing::warn!(
                "recording {id}: segment file {} not found, skipping",
                segment_path.display()
            );
            continue;
        }
        let bytes = std::fs::read(&segment_path).with_context(|| {
            format!(
                "failed reading recording segment {}",
                segment_path.display()
            )
        })?;
        let result = bmux_ipc::read_frames(&bytes).map_err(|e| {
            anyhow::anyhow!(
                "failed parsing recording segment {}: {e}",
                segment_path.display()
            )
        })?;
        if result.bytes_remaining > 0 {
            tracing::warn!(
                "recording {id}: segment {} has {} trailing bytes (truncated?)",
                segment_name,
                result.bytes_remaining
            );
        }
        all_frames.extend(result.frames);
    }

    Ok(all_frames)
}

pub(super) fn resolve_recording_id_prefix(
    value: &str,
    recordings: &[RecordingSummary],
) -> Result<Uuid> {
    if let Ok(id) = Uuid::parse_str(value) {
        if recordings.iter().any(|recording| recording.id == id) {
            return Ok(id);
        }
        anyhow::bail!("recording not found: {id}");
    }

    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        anyhow::bail!("recording id/prefix cannot be empty");
    }

    let matches = recordings
        .iter()
        .filter_map(|recording| {
            let id = recording.id.to_string();
            id.starts_with(&normalized).then_some(recording.id)
        })
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [id] => Ok(*id),
        [] => anyhow::bail!("no recording matches prefix '{value}'"),
        _ => {
            let mut options = matches
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>();
            options.sort();
            anyhow::bail!(
                "recording prefix '{value}' is ambiguous; matches: {}",
                options.join(", ")
            )
        }
    }
}

pub(super) fn delete_recording_dir(recording_id: Uuid) -> Result<()> {
    delete_recording_dir_at(&recordings_root_dir(), recording_id)
}

pub(super) fn delete_recording_dir_at(recordings_root: &Path, recording_id: Uuid) -> Result<()> {
    let dir = recordings_root.join(recording_id.to_string());
    let manifest = dir.join("manifest.json");
    if !manifest.exists() {
        anyhow::bail!("recording not found: {recording_id}");
    }
    std::fs::remove_dir_all(&dir)
        .with_context(|| format!("failed removing recording directory {}", dir.display()))?;
    Ok(())
}

pub(super) fn delete_all_recordings_from_disk() -> Result<usize> {
    delete_all_recordings_from_dir(&recordings_root_dir())
}

pub(super) fn delete_all_recordings_from_dir(root: &Path) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }

    let mut deleted_count = 0_usize;
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("failed reading recordings dir {}", root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let manifest = entry.path().join("manifest.json");
        if !manifest.exists() {
            continue;
        }
        std::fs::remove_dir_all(entry.path()).with_context(|| {
            format!(
                "failed removing recording directory {}",
                entry.path().display()
            )
        })?;
        deleted_count = deleted_count.saturating_add(1);
    }
    Ok(deleted_count)
}

pub(super) fn confirm_delete_all_recordings(yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        anyhow::bail!("recording delete-all requires --yes in non-interactive mode");
    }

    println!("Delete all recordings? [y/N]");
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("failed reading delete-all confirmation")?;
    let trimmed = answer.trim().to_ascii_lowercase();
    Ok(trimmed == "y" || trimmed == "yes")
}

#[derive(Debug, serde::Deserialize)]
struct RecordingManifest {
    summary: RecordingSummary,
}

fn read_recording_manifest(manifest_path: &Path) -> Result<RecordingSummary> {
    let bytes = std::fs::read(manifest_path).with_context(|| {
        format!(
            "failed reading recording manifest {}",
            manifest_path.display()
        )
    })?;
    let manifest: RecordingManifest = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "failed parsing recording manifest {}",
            manifest_path.display()
        )
    })?;
    Ok(manifest.summary)
}

pub(super) fn list_recordings_from_disk() -> Result<Vec<RecordingSummary>> {
    list_recordings_from_dir(&recordings_root_dir())
}

pub(super) fn recordings_root_dir() -> PathBuf {
    let paths = ConfigPaths::default();
    BmuxConfig::load_from_path(&paths.config_file()).map_or_else(
        |_| paths.recordings_dir(),
        |config| config.recordings_dir(&paths),
    )
}

pub(super) fn list_recordings_from_dir(recordings_root: &Path) -> Result<Vec<RecordingSummary>> {
    if !recordings_root.exists() {
        return Ok(Vec::new());
    }

    let mut recordings = Vec::new();
    for entry in std::fs::read_dir(recordings_root).with_context(|| {
        format!(
            "failed reading recordings dir {}",
            recordings_root.display()
        )
    })? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let manifest_path = entry.path().join("manifest.json");
        if !manifest_path.exists() {
            continue;
        }
        if let Ok(summary) = read_recording_manifest(&manifest_path) {
            recordings.push(summary);
        }
    }

    recordings.sort_by(|a, b| b.started_epoch_ms.cmp(&a.started_epoch_ms));
    Ok(recordings)
}

pub(super) const fn offline_recording_status() -> RecordingStatus {
    RecordingStatus {
        active: None,
        queue_len: 0,
    }
}

// Display track types are defined in bmux_ipc for cross-module sharing.
use bmux_ipc::{DisplayTrackEnvelope, DisplayTrackEvent};

pub(super) struct DisplayCaptureWriter {
    started_at: Instant,
    writer: BufWriter<std::fs::File>,
}

impl DisplayCaptureWriter {
    pub(super) fn new(
        plan: Option<AttachDisplayCapturePlan>,
        client_id: Uuid,
    ) -> Result<Option<Self>> {
        let Some(plan) = plan else {
            return Ok(None);
        };
        std::fs::create_dir_all(&plan.recording_path).with_context(|| {
            format!(
                "failed creating recording path {}",
                plan.recording_path.display()
            )
        })?;
        write_recording_owner_client(&plan.recording_path, client_id)?;
        let display_track_path = display_track_path(&plan.recording_path, client_id);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&display_track_path)
            .with_context(|| {
                format!(
                    "failed opening display track {}",
                    display_track_path.display()
                )
            })?;
        let mut capture = Self {
            started_at: Instant::now(),
            writer: BufWriter::new(file),
        };
        let (cell_width_px, cell_height_px, window_width_px, window_height_px) =
            capture_stream_open_metrics();
        let terminal_profile = terminal_profile::detect_render_profile();
        let terminal_profile_bytes = terminal_profile
            .as_ref()
            .and_then(|p| bmux_ipc::encode(p).ok());
        capture.record(DisplayTrackEvent::StreamOpened {
            client_id,
            recording_id: plan.recording_id,
            cell_width_px,
            cell_height_px,
            window_width_px,
            window_height_px,
            terminal_profile: terminal_profile_bytes,
        })?;
        if let Ok((cols, rows)) = terminal::size()
            && cols > 0
            && rows > 0
        {
            capture.record(DisplayTrackEvent::Resize { cols, rows })?;
        }
        Ok(Some(capture))
    }

    pub(super) fn record_resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.record(DisplayTrackEvent::Resize { cols, rows })
    }

    pub(super) fn record_frame_bytes(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.record(DisplayTrackEvent::FrameBytes {
            data: data.to_vec(),
        })
    }

    pub(super) fn record_stream_closed(&mut self) -> Result<()> {
        self.record(DisplayTrackEvent::StreamClosed)
    }

    pub(super) fn flush(&mut self) -> Result<()> {
        self.writer
            .flush()
            .context("failed flushing display capture writer")
    }

    fn record(&mut self, event: DisplayTrackEvent) -> Result<()> {
        let envelope = DisplayTrackEnvelope {
            mono_ns: self
                .started_at
                .elapsed()
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64,
            event,
        };
        bmux_ipc::write_frame(&mut self.writer, &envelope)
            .map_err(|e| anyhow::anyhow!("display track write_frame failed: {e}"))?;
        Ok(())
    }
}

fn display_track_path(recording_path: &Path, client_id: Uuid) -> PathBuf {
    recording_path.join(format!("display-{client_id}.bin"))
}

fn write_recording_owner_client(recording_path: &Path, client_id: Uuid) -> Result<()> {
    let owner_path = recording_path.join("owner-client-id.txt");
    std::fs::write(&owner_path, format!("{client_id}\n"))
        .with_context(|| format!("failed writing owner client file {}", owner_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_opened(
        cell_width_px: Option<u16>,
        cell_height_px: Option<u16>,
        window_width_px: Option<u16>,
        window_height_px: Option<u16>,
    ) -> DisplayTrackEnvelope {
        DisplayTrackEnvelope {
            mono_ns: 1,
            event: DisplayTrackEvent::StreamOpened {
                client_id: Uuid::nil(),
                recording_id: Uuid::nil(),
                cell_width_px,
                cell_height_px,
                window_width_px,
                window_height_px,
                terminal_profile: None,
            },
        }
    }

    fn frame_bytes(mono_ns: u64) -> DisplayTrackEnvelope {
        DisplayTrackEnvelope {
            mono_ns,
            event: DisplayTrackEvent::FrameBytes { data: vec![b'x'] },
        }
    }

    #[test]
    fn display_track_envelope_round_trips_through_codec() {
        let envelope = DisplayTrackEnvelope {
            mono_ns: 1,
            event: DisplayTrackEvent::StreamOpened {
                client_id: Uuid::nil(),
                recording_id: Uuid::nil(),
                cell_width_px: Some(8),
                cell_height_px: Some(16),
                window_width_px: Some(640),
                window_height_px: Some(480),
                terminal_profile: None,
            },
        };
        let mut buf = Vec::new();
        bmux_ipc::write_frame(&mut buf, &envelope).expect("write should succeed");
        let result =
            bmux_ipc::read_frames::<DisplayTrackEnvelope>(&buf).expect("read should succeed");
        assert_eq!(result.bytes_remaining, 0);
        assert_eq!(result.frames.len(), 1);
        assert_eq!(result.frames[0].mono_ns, 1);
        match &result.frames[0].event {
            DisplayTrackEvent::StreamOpened {
                cell_width_px,
                cell_height_px,
                ..
            } => {
                assert_eq!(*cell_width_px, Some(8));
                assert_eq!(*cell_height_px, Some(16));
            }
            _ => panic!("expected stream_opened event"),
        }
    }

    #[test]
    fn resolve_export_cell_metrics_prefers_cli_then_recording() {
        let events = vec![stream_opened(Some(7), Some(14), Some(700), Some(350))];
        let resolved = resolve_export_cell_metrics(&events, Some((9, 18)), Some(10), None)
            .expect("metrics should resolve");
        assert_eq!(resolved.width, 10);
        assert_eq!(resolved.height, 18);
    }

    #[test]
    fn resolve_export_cell_metrics_can_infer_from_recorded_window_and_resize() {
        let events = vec![
            stream_opened(None, None, Some(1200), Some(600)),
            DisplayTrackEnvelope {
                mono_ns: 2,
                event: DisplayTrackEvent::Resize {
                    cols: 120,
                    rows: 30,
                },
            },
        ];
        let resolved =
            resolve_export_cell_metrics(&events, None, None, None).expect("metrics should resolve");
        assert_eq!(resolved.width, 10);
        assert_eq!(resolved.height, 20);
    }

    #[test]
    fn build_render_options_uses_terminal_profile_defaults() {
        let profile = terminal_profile::DetectedTerminalProfile {
            terminal_id: "ghostty".to_string(),
            font_families: vec!["JetBrains Mono".to_string()],
            font_size_px: Some(15),
            background_opacity_permille: Some(900),
            source: "test".to_string(),
        };
        let options = build_render_options(
            Some(&profile),
            RecordingRenderMode::Bitmap,
            None,
            None,
            None,
            &[],
        )
        .expect("options should resolve");
        assert_eq!(options.font_families, vec!["JetBrains Mono".to_string()]);
        assert_eq!(options.font_size_px, Some(15.0));
        assert_eq!(options.line_height_mult, 1.0);
        assert_eq!(options.background_opacity, 0.9);
        assert_eq!(options.backdrop_rgb, (0, 0, 0));
    }

    #[test]
    fn recording_terminal_profile_reads_stream_opened_profile() {
        let profile = terminal_profile::DetectedTerminalProfile {
            terminal_id: "ghostty".to_string(),
            font_families: vec!["Iosevka".to_string()],
            font_size_px: Some(14),
            background_opacity_permille: None,
            source: "ghostty-config:/tmp/config".to_string(),
        };
        let events = vec![DisplayTrackEnvelope {
            mono_ns: 1,
            event: DisplayTrackEvent::StreamOpened {
                client_id: Uuid::nil(),
                recording_id: Uuid::nil(),
                cell_width_px: Some(8),
                cell_height_px: Some(16),
                window_width_px: Some(800),
                window_height_px: Some(600),
                terminal_profile: Some(bmux_ipc::encode(&profile).unwrap()),
            },
        }];
        let resolved = recording_terminal_profile(&events).expect("profile should be resolved");
        assert_eq!(resolved, profile);
    }

    #[test]
    fn bitmap_glyph_cache_reuses_computed_mask() {
        let mut cache = BitmapGlyphCache::new(8, 16);
        let first = cache.mask_for('A').expect("mask should exist").to_vec();
        let second = cache.mask_for('A').expect("mask should exist").to_vec();
        assert_eq!(first, second);
        assert_eq!(cache.masks.len(), 1);
    }

    #[test]
    fn estimate_export_progress_counts_events_and_emitted_frames() {
        let events = vec![
            stream_opened(Some(8), Some(16), Some(800), Some(600)),
            frame_bytes(0),
            frame_bytes(50_000_000),
            frame_bytes(100_000_000),
            frame_bytes(200_000_000),
        ];
        let estimate = estimate_export_progress(&events, 1.0, 10, None, None);
        assert_eq!(estimate.total_frame_events, 4);
        assert_eq!(estimate.estimated_emitted_frames, 3);
    }

    #[test]
    fn estimate_export_progress_respects_max_frames_limit() {
        let events = vec![
            stream_opened(Some(8), Some(16), Some(800), Some(600)),
            frame_bytes(0),
            frame_bytes(50_000_000),
            frame_bytes(100_000_000),
            frame_bytes(200_000_000),
        ];
        let estimate = estimate_export_progress(&events, 1.0, 10, None, Some(2));
        assert_eq!(estimate.total_frame_events, 3);
        assert_eq!(estimate.estimated_emitted_frames, 2);
    }

    #[test]
    fn estimate_export_progress_uses_timeline_frames_for_sparse_events() {
        let events = vec![
            stream_opened(Some(8), Some(16), Some(800), Some(600)),
            frame_bytes(0),
            frame_bytes(450_000_000),
        ];
        let estimate = estimate_export_progress(&events, 1.0, 10, None, None);
        assert_eq!(estimate.total_frame_events, 2);
        assert_eq!(estimate.estimated_emitted_frames, 5);
    }

    #[test]
    fn compute_cursor_visibility_blinks_on_timeline_clock() {
        let options = CursorExportOptions {
            mode: RecordingCursorMode::Auto,
            shape: RecordingCursorShape::Auto,
            blink: RecordingCursorBlinkMode::On,
            blink_period_ns: 500_000_000,
            color_override: None,
        };
        let state = CursorReplayState::default();
        let mut blink_anchor_ns = None;
        let (on_a, blink_a) =
            compute_cursor_visibility(&options, state, true, 0, &mut blink_anchor_ns);
        let (on_b, blink_b) =
            compute_cursor_visibility(&options, state, true, 510_000_000, &mut blink_anchor_ns);
        let (on_c, blink_c) =
            compute_cursor_visibility(&options, state, true, 1_020_000_000, &mut blink_anchor_ns);
        assert!(on_a && blink_a);
        assert!(!on_b && !blink_b);
        assert!(on_c && blink_c);
    }

    #[test]
    fn compute_cursor_visibility_aligns_phase_to_first_visible_frame() {
        let options = CursorExportOptions {
            mode: RecordingCursorMode::Auto,
            shape: RecordingCursorShape::Auto,
            blink: RecordingCursorBlinkMode::On,
            blink_period_ns: 500_000_000,
            color_override: None,
        };
        let state = CursorReplayState::default();
        let mut blink_anchor_ns = None;
        let _ =
            compute_cursor_visibility(&options, state, false, 700_000_000, &mut blink_anchor_ns);
        let (on_a, blink_a) =
            compute_cursor_visibility(&options, state, true, 700_000_000, &mut blink_anchor_ns);
        let (on_b, blink_b) =
            compute_cursor_visibility(&options, state, true, 1_210_000_000, &mut blink_anchor_ns);
        assert!(on_a && blink_a);
        assert!(!on_b && !blink_b);
    }

    #[test]
    fn format_duration_compact_uses_mm_ss_and_hh_mm_ss() {
        assert_eq!(
            format_duration_compact(std::time::Duration::from_secs(65)),
            "01:05"
        );
        assert_eq!(
            format_duration_compact(std::time::Duration::from_secs(3_665)),
            "1:01:05"
        );
    }

    #[test]
    fn parse_cursor_color_accepts_auto_and_hex() {
        assert_eq!(parse_cursor_color("auto").expect("auto should parse"), None);
        assert_eq!(
            parse_cursor_color("#11AAee").expect("hex should parse"),
            Some((0x11, 0xaa, 0xee))
        );
    }

    #[test]
    fn update_cursor_replay_state_parses_decscusr() {
        let mut state = CursorReplayState::default();
        update_cursor_replay_state(&mut state, b"\x1b[6 q");
        assert!(matches!(state.shape, CursorVisualShape::Bar));
        assert!(!state.blink_enabled);
        update_cursor_replay_state(&mut state, b"\x1b[3 q");
        assert!(matches!(state.shape, CursorVisualShape::Underline));
        assert!(state.blink_enabled);
    }
}
