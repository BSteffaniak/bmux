use ab_glyph::{Font, FontArc, FontVec, PxScale, ScaleFont, point};
use anyhow::{Context, Result};
use bmux_fonts::FontPreset;
use bmux_recording_protocol::{
    DisplayActivityKind, DisplayCursorShape, DisplayTrackEnvelope, DisplayTrackEvent,
    RECORDING_FORMAT_VERSION, read_frames,
};
use font8x8::UnicodeFonts;
use gif::{Encoder as GifEncoder, Frame as GifFrame, Repeat};
use resvg::{tiny_skia, usvg};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use uuid::Uuid;

mod terminal_profile;

const GIF_QUANTIZATION_SAMPLE_FACTOR: i32 = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordingExportFormat {
    Gif,
}

// Native command parsing will construct the non-default variants; auto-export uses defaults.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordingRenderMode {
    Font,
    Bitmap,
}

// Native command parsing will construct the non-default variants; auto-export uses defaults.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordingPaletteSource {
    Auto,
    Recording,
    Terminal,
    Xterm,
}

// Native command parsing will construct the non-default variants; auto-export uses defaults.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordingCursorMode {
    Auto,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordingCursorShape {
    Auto,
    Block,
    Bar,
    Underline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordingCursorBlinkMode {
    Auto,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordingCursorProfile {
    Auto,
    Ghostty,
    Generic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordingCursorPaintMode {
    Auto,
    Invert,
    Fill,
    Outline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordingCursorTextMode {
    Auto,
    SwapFgBg,
    ForceContrast,
}

pub(crate) fn export_recording_gif_from_root(
    recordings_root: &Path,
    recording_id_or_prefix: &str,
    output: &str,
    fps: u32,
) -> Result<()> {
    let (recording_id, recording_dir) =
        resolve_recording_dir(recordings_root, recording_id_or_prefix)?;
    export_recording_gif_for_recording_dir(&recording_dir, recording_id, output, fps)
}

pub(crate) fn export_recording_gif_for_recording_dir(
    recording_dir: &Path,
    recording_id: Uuid,
    output: &str,
    fps: u32,
) -> Result<()> {
    export_recording_for_recording_dir(
        recording_dir,
        recording_id,
        RecordingExportFormat::Gif,
        output,
        None,
        1.0,
        fps,
        None,
        None,
        RecordingRenderMode::Font,
        None,
        None,
        None,
        None,
        None,
        None,
        &[],
        RecordingPaletteSource::Auto,
        None,
        None,
        &[],
        RecordingCursorMode::Auto,
        RecordingCursorShape::Auto,
        RecordingCursorBlinkMode::Auto,
        500,
        "auto",
        RecordingCursorProfile::Auto,
        None,
        None,
        None,
        None,
        RecordingCursorPaintMode::Auto,
        RecordingCursorTextMode::Auto,
        10,
        8,
        None,
        false,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn export_recording_for_recording_dir(
    recording_dir: &Path,
    recording_id: Uuid,
    format: RecordingExportFormat,
    output: &str,
    view_client: Option<Uuid>,
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
    palette_source: RecordingPaletteSource,
    palette_foreground: Option<&str>,
    palette_background: Option<&str>,
    palette_color: &[String],
    cursor: RecordingCursorMode,
    cursor_shape: RecordingCursorShape,
    cursor_blink: RecordingCursorBlinkMode,
    cursor_blink_period_ms: u32,
    cursor_color: &str,
    cursor_profile: RecordingCursorProfile,
    cursor_solid_after_activity_ms: Option<u32>,
    cursor_solid_after_input_ms: Option<u32>,
    cursor_solid_after_output_ms: Option<u32>,
    cursor_solid_after_cursor_ms: Option<u32>,
    cursor_paint_mode: RecordingCursorPaintMode,
    cursor_text_mode: RecordingCursorTextMode,
    cursor_bar_width_pct: u8,
    cursor_underline_height_pct: u8,
    export_metadata: Option<&str>,
    show_progress: bool,
) -> Result<()> {
    if !recording_dir.exists() {
        anyhow::bail!("recording not found: {recording_id}")
    }
    ensure_supported_manifest(recording_dir)?;

    let selected_client = if let Some(id) = view_client {
        id
    } else if let Some(owner) = read_recording_owner_client(recording_dir)? {
        owner
    } else {
        match infer_display_track_client(recording_dir) {
            InferredClient::One(id) => id,
            InferredClient::Multiple(ids) => {
                anyhow::bail!(
                    "multiple display tracks found; pass --view-client with one of: {}",
                    ids.iter()
                        .map(std::string::ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            InferredClient::None => {
                anyhow::bail!("no display tracks found in recording; cannot export");
            }
        }
    };

    let events = load_display_track_events(recording_dir, selected_client)?;
    if events.is_empty() {
        anyhow::bail!(
            "display track is empty for client {selected_client}; cannot export exact-view media"
        )
    }

    let recording_profile = recording_terminal_profile(&events);
    let host_profile = terminal_profile::detect_render_profile();
    let terminal_profile = recording_profile.as_ref().or(host_profile.as_ref());

    match format {
        RecordingExportFormat::Gif => export_recording_gif(
            &events,
            output,
            speed,
            fps,
            max_duration,
            max_frames,
            terminal_profile,
            recording_profile.as_ref(),
            host_profile.as_ref(),
            renderer,
            cell_size,
            cell_width,
            cell_height,
            font_family,
            font_size,
            line_height,
            font_path,
            palette_source,
            palette_foreground,
            palette_background,
            palette_color,
            cursor,
            cursor_shape,
            cursor_blink,
            cursor_blink_period_ms,
            cursor_color,
            cursor_profile,
            cursor_solid_after_activity_ms,
            cursor_solid_after_input_ms,
            cursor_solid_after_output_ms,
            cursor_solid_after_cursor_ms,
            cursor_paint_mode,
            cursor_text_mode,
            cursor_bar_width_pct,
            cursor_underline_height_pct,
            export_metadata,
            show_progress,
        )?,
    }

    tracing::info!(
        recording_id = %recording_id,
        format = ?format,
        view_client = %selected_client,
        output,
        "recording export completed"
    );
    Ok(())
}

fn ensure_supported_manifest(recording_dir: &Path) -> Result<()> {
    #[derive(serde::Deserialize)]
    struct ManifestFormat {
        summary: ManifestSummary,
    }

    #[derive(serde::Deserialize)]
    struct ManifestSummary {
        format_version: u32,
    }

    let manifest_path = recording_dir.join("manifest.json");
    let manifest: ManifestFormat =
        serde_json::from_slice(&std::fs::read(&manifest_path).with_context(|| {
            format!(
                "failed reading recording manifest {}",
                manifest_path.display()
            )
        })?)
        .with_context(|| {
            format!(
                "failed parsing recording manifest {}",
                manifest_path.display()
            )
        })?;
    if manifest.summary.format_version != RECORDING_FORMAT_VERSION {
        anyhow::bail!(
            "recording format version {} is unsupported; expected {}. re-record with current bmux",
            manifest.summary.format_version,
            RECORDING_FORMAT_VERSION
        );
    }
    Ok(())
}

fn parse_uuid_value(value: &str, label: &str) -> Result<Uuid> {
    Uuid::parse_str(value).with_context(|| format!("invalid {label} UUID: {value}"))
}

fn resolve_recording_dir(recordings_root: &Path, value: &str) -> Result<(Uuid, PathBuf)> {
    if let Ok(id) = Uuid::parse_str(value) {
        let dir = recordings_root.join(id.to_string());
        if dir.exists() {
            return Ok((id, dir));
        }
    }

    let normalized = value.to_ascii_lowercase();
    let mut matches = Vec::new();
    for entry in std::fs::read_dir(recordings_root)
        .with_context(|| {
            format!(
                "failed reading recordings root {}",
                recordings_root.display()
            )
        })?
        .flatten()
    {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.to_ascii_lowercase().starts_with(&normalized)
            && let Ok(id) = Uuid::parse_str(&name)
        {
            matches.push((id, entry.path()));
        }
    }

    match matches.as_slice() {
        [(id, path)] => Ok((*id, path.clone())),
        [] => anyhow::bail!("no recording matches id prefix '{value}'"),
        _ => anyhow::bail!("recording id prefix '{value}' is ambiguous"),
    }
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

enum InferredClient {
    One(Uuid),
    Multiple(Vec<Uuid>),
    None,
}

/// When `owner-client-id.txt` is missing, scan the recording directory for
/// `display-{uuid}.bin` files. If exactly one exists, return its client id so
/// the export can proceed without requiring `--view-client`.
fn infer_display_track_client(recording_dir: &Path) -> InferredClient {
    let Ok(entries) = std::fs::read_dir(recording_dir) else {
        return InferredClient::None;
    };
    let mut found: Vec<Uuid> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(rest) = name.strip_prefix("display-")
            && let Some(uuid_str) = rest.strip_suffix(".bin")
            && let Ok(id) = uuid_str.parse::<Uuid>()
        {
            found.push(id);
        }
    }
    match found.len() {
        1 => InferredClient::One(found[0]),
        n if n > 1 => InferredClient::Multiple(found),
        _ => InferredClient::None,
    }
}

fn load_display_track_events(
    recording_dir: &Path,
    client_id: Uuid,
) -> Result<Vec<DisplayTrackEnvelope>> {
    let path = display_track_path(recording_dir, client_id);
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed reading display track {}", path.display()))?;
    let result = read_frames(&bytes)
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

fn display_track_path(recording_path: &Path, client_id: Uuid) -> PathBuf {
    recording_path.join(format!("display-{client_id}.bin"))
}

fn recording_terminal_profile(
    events: &[DisplayTrackEnvelope],
) -> Option<terminal_profile::DetectedTerminalProfile> {
    for envelope in events {
        if let DisplayTrackEvent::StreamOpened {
            terminal_profile: Some(profile_bytes),
            ..
        } = &envelope.event
            && let Ok(profile) =
                bmux_ipc::decode::<terminal_profile::DetectedTerminalProfile>(profile_bytes)
        {
            return Some(profile);
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
    let cli_width = cell_width.or_else(|| (size_width > 0).then_some(size_width));
    let cli_height = cell_height.or_else(|| (size_height > 0).then_some(size_height));

    let recorded = recording_cell_metrics(events);
    let current = current_terminal_cell_metrics();
    let width = cli_width
        .or_else(|| recorded.map(|value| value.width))
        .or_else(|| current.map(|value| value.width))
        .unwrap_or(8);
    let height = cli_height
        .or_else(|| recorded.map(|value| value.height))
        .or_else(|| current.map(|value| value.height))
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
            DisplayTrackEvent::FrameBytes { .. }
            | DisplayTrackEvent::CursorSnapshot { .. }
            | DisplayTrackEvent::Activity { .. }
            | DisplayTrackEvent::ImageUpdate { .. }
            | DisplayTrackEvent::StreamClosed => {}
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
    // Plugin exports run in the server/job worker, not an attached user TTY.
    // Use recorded stream metrics when available, then deterministic defaults.
    None
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

fn infer_export_terminal_bounds(events: &[DisplayTrackEnvelope]) -> Result<(u16, u16)> {
    let mut resize_bounds = None::<(u16, u16)>;
    let mut stream_bounds = None::<(u16, u16)>;
    let mut cursor_cols = 0_u16;
    let mut cursor_rows = 0_u16;

    for envelope in events {
        match envelope.event {
            DisplayTrackEvent::Resize { cols, rows } => {
                let cols = cols.max(1);
                let rows = rows.max(1);
                resize_bounds = Some(match resize_bounds {
                    Some((current_cols, current_rows)) => {
                        (current_cols.max(cols), current_rows.max(rows))
                    }
                    None => (cols, rows),
                });
            }
            DisplayTrackEvent::StreamOpened {
                cell_width_px,
                cell_height_px,
                window_width_px,
                window_height_px,
                ..
            } => {
                if let (
                    Some(cell_width),
                    Some(cell_height),
                    Some(window_width),
                    Some(window_height),
                ) = (
                    cell_width_px,
                    cell_height_px,
                    window_width_px,
                    window_height_px,
                ) && cell_width > 0
                    && cell_height > 0
                {
                    let cols = (window_width / cell_width).max(1);
                    let rows = (window_height / cell_height).max(1);
                    stream_bounds = Some(match stream_bounds {
                        Some((current_cols, current_rows)) => {
                            (current_cols.max(cols), current_rows.max(rows))
                        }
                        None => (cols, rows),
                    });
                }
            }
            DisplayTrackEvent::CursorSnapshot { x, y, .. } => {
                cursor_cols = cursor_cols.max(x.saturating_add(1));
                cursor_rows = cursor_rows.max(y.saturating_add(1));
            }
            DisplayTrackEvent::FrameBytes { .. }
            | DisplayTrackEvent::Activity { .. }
            | DisplayTrackEvent::ImageUpdate { .. }
            | DisplayTrackEvent::StreamClosed => {}
        }
    }

    if let Some((cols, rows)) = resize_bounds {
        return Ok((cols, rows));
    }

    if let Some((stream_cols, stream_rows)) = stream_bounds {
        return Ok((stream_cols.max(cursor_cols), stream_rows.max(cursor_rows)));
    }

    anyhow::bail!(
        "recording export cannot infer terminal bounds: display track is missing resize events and stream-opened grid metrics"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    profile: RecordingCursorProfile,
    blink_period_ns: u64,
    solid_after_input_ns: u64,
    solid_after_output_ns: u64,
    solid_after_cursor_ns: u64,
    paint_mode: RecordingCursorPaintMode,
    text_mode: RecordingCursorTextMode,
    bar_width_pct: u8,
    underline_height_pct: u8,
    color_label: String,
    color_override: Option<(u8, u8, u8)>,
}

#[derive(Debug, Clone, Copy)]
struct RecordedCursorSnapshot {
    x: u16,
    y: u16,
    visible: bool,
    shape: DisplayCursorShape,
    blink_enabled: bool,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum CursorVisibilityReason {
    Hidden,
    ForcedOn,
    HoldInput,
    HoldOutput,
    HoldCursor,
    BlinkOn,
    BlinkOff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockPaintMode {
    Invert,
    Fill,
    Outline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockTextMode {
    SwapFgBg,
    ForceContrast,
}

#[derive(Debug, Clone, serde::Serialize)]
struct ExportCursorFrame {
    mono_ns: u64,
    row: u16,
    col: u16,
    visible: bool,
    shape: &'static str,
    blink_on: bool,
    cursor_source: &'static str,
    visible_reason: CursorVisibilityReason,
    paint_mode_used: &'static str,
    text_mode_used: &'static str,
    paint_fallback_reason: Option<&'static str>,
    last_input_activity_ns: Option<u64>,
    last_output_activity_ns: Option<u64>,
    last_cursor_activity_ns: Option<u64>,
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
    profile: &'a str,
    blink_period_ms: u32,
    solid_after_input_ms: u32,
    solid_after_output_ms: u32,
    solid_after_cursor_ms: u32,
    paint_mode: &'a str,
    text_mode: &'a str,
    bar_width_pct: u8,
    underline_height_pct: u8,
    color: &'a str,
}

fn parse_cursor_color(value: &str) -> Result<Option<(u8, u8, u8)>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    let Some(rgb) = parse_rgb_color(trimmed) else {
        anyhow::bail!("invalid cursor color '{value}'; expected auto or a color value")
    };
    Ok(Some(rgb))
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

const fn display_cursor_shape_from_visual(shape: CursorVisualShape) -> DisplayCursorShape {
    match shape {
        CursorVisualShape::Block => DisplayCursorShape::Block,
        CursorVisualShape::Bar => DisplayCursorShape::Bar,
        CursorVisualShape::Underline => DisplayCursorShape::Underline,
    }
}

fn cursor_snapshot_from_grid_fallback(
    grid: &bmux_terminal_grid::TerminalGrid,
    replay_state: CursorReplayState,
) -> RecordedCursorSnapshot {
    let cursor = grid.cursor();
    RecordedCursorSnapshot {
        x: u16::try_from(cursor.col).unwrap_or(u16::MAX),
        y: u16::try_from(cursor.row).unwrap_or(u16::MAX),
        visible: cursor.visible,
        shape: display_cursor_shape_from_visual(replay_state.shape),
        blink_enabled: replay_state.blink_enabled,
    }
}

const fn effective_cursor_shape(
    options: &CursorExportOptions,
    replay_state: CursorReplayState,
    snapshot_shape: DisplayCursorShape,
) -> CursorVisualShape {
    match options.shape {
        RecordingCursorShape::Auto => match snapshot_shape {
            DisplayCursorShape::Block => replay_state.shape,
            DisplayCursorShape::Bar => CursorVisualShape::Bar,
            DisplayCursorShape::Underline => CursorVisualShape::Underline,
        },
        RecordingCursorShape::Block => CursorVisualShape::Block,
        RecordingCursorShape::Bar => CursorVisualShape::Bar,
        RecordingCursorShape::Underline => CursorVisualShape::Underline,
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn compute_cursor_visibility(
    options: &CursorExportOptions,
    replay_state: CursorReplayState,
    snapshot_blink_enabled: bool,
    parser_visible: bool,
    mono_ns: u64,
    last_input_activity_ns: Option<u64>,
    last_output_activity_ns: Option<u64>,
    last_cursor_activity_ns: Option<u64>,
    blink_anchor_ns: &mut Option<u64>,
) -> (bool, bool, CursorVisibilityReason) {
    let base_visible = match options.mode {
        RecordingCursorMode::Auto => parser_visible,
        RecordingCursorMode::On => true,
        RecordingCursorMode::Off => false,
    };
    if !base_visible {
        return (false, true, CursorVisibilityReason::Hidden);
    }
    if matches!(options.mode, RecordingCursorMode::On) {
        return (true, true, CursorVisibilityReason::ForcedOn);
    }
    let blink_enabled = match options.blink {
        RecordingCursorBlinkMode::Auto => replay_state.blink_enabled && snapshot_blink_enabled,
        RecordingCursorBlinkMode::On => true,
        RecordingCursorBlinkMode::Off => false,
    };
    if !blink_enabled {
        return (true, true, CursorVisibilityReason::ForcedOn);
    }
    if last_input_activity_ns
        .is_some_and(|last| mono_ns.saturating_sub(last) < options.solid_after_input_ns)
    {
        return (true, true, CursorVisibilityReason::HoldInput);
    }
    if last_output_activity_ns
        .is_some_and(|last| mono_ns.saturating_sub(last) < options.solid_after_output_ns)
    {
        return (true, true, CursorVisibilityReason::HoldOutput);
    }
    if last_cursor_activity_ns
        .is_some_and(|last| mono_ns.saturating_sub(last) < options.solid_after_cursor_ns)
    {
        return (true, true, CursorVisibilityReason::HoldCursor);
    }
    let latest_activity = [
        last_input_activity_ns,
        last_output_activity_ns,
        last_cursor_activity_ns,
    ]
    .into_iter()
    .flatten()
    .max();
    if let Some(last_activity) = latest_activity
        && matches!(options.profile, RecordingCursorProfile::Ghostty)
        && last_activity <= mono_ns
        && blink_anchor_ns.is_none_or(|anchor| last_activity > anchor)
    {
        *blink_anchor_ns = Some(last_activity);
    }
    let period = options.blink_period_ns.max(1);
    let anchor = *blink_anchor_ns.get_or_insert(mono_ns);
    let phase_ns = mono_ns.saturating_sub(anchor);
    let blink_on = (phase_ns / period).is_multiple_of(2);
    (
        blink_on,
        blink_on,
        if blink_on {
            CursorVisibilityReason::BlinkOn
        } else {
            CursorVisibilityReason::BlinkOff
        },
    )
}

const fn cursor_shape_name(shape: CursorVisualShape) -> &'static str {
    match shape {
        CursorVisualShape::Block => "block",
        CursorVisualShape::Bar => "bar",
        CursorVisualShape::Underline => "underline",
    }
}

const fn paint_mode_name(mode: BlockPaintMode) -> &'static str {
    match mode {
        BlockPaintMode::Invert => "invert",
        BlockPaintMode::Fill => "fill",
        BlockPaintMode::Outline => "outline",
    }
}

const fn text_mode_name(mode: BlockTextMode) -> &'static str {
    match mode {
        BlockTextMode::SwapFgBg => "swap_fg_bg",
        BlockTextMode::ForceContrast => "force_contrast",
    }
}

fn relative_luminance(rgb: (u8, u8, u8)) -> f32 {
    let channel = |value: u8| {
        let v = f32::from(value) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.0722f32.mul_add(
        channel(rgb.2),
        0.2126f32.mul_add(channel(rgb.0), 0.7152 * channel(rgb.1)),
    )
}

fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    let l1 = relative_luminance(a);
    let l2 = relative_luminance(b);
    let (high, low) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (high + 0.05) / (low + 0.05)
}

fn pick_contrast_text_color(fill: (u8, u8, u8)) -> (u8, u8, u8) {
    if contrast_ratio((0, 0, 0), fill) >= contrast_ratio((255, 255, 255), fill) {
        (0, 0, 0)
    } else {
        (255, 255, 255)
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn overlay_cursor_rgba(
    pixels: &mut [u8],
    frame_width: usize,
    frame_height: usize,
    cell_w: usize,
    cell_h: usize,
    row: u16,
    col: u16,
    shape: CursorVisualShape,
    paint_mode: BlockPaintMode,
    text_mode: BlockTextMode,
    bar_width_pct: u8,
    underline_height_pct: u8,
    cell_foreground: (u8, u8, u8),
    cell_background: (u8, u8, u8),
    color: (u8, u8, u8),
) -> (BlockPaintMode, BlockTextMode, Option<&'static str>) {
    if frame_width == 0 || frame_height == 0 || cell_w == 0 || cell_h == 0 {
        return (paint_mode, text_mode, None);
    }
    let x0 = usize::from(col).saturating_mul(cell_w);
    let y0 = usize::from(row).saturating_mul(cell_h);
    if x0 >= frame_width || y0 >= frame_height {
        return (paint_mode, text_mode, None);
    }
    let resolved_paint_mode = paint_mode;
    let mut resolved_text_mode = text_mode;
    let mut fallback_reason = None;
    match shape {
        CursorVisualShape::Block => match resolved_paint_mode {
            BlockPaintMode::Invert => {
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
            BlockPaintMode::Fill => {
                let mut effective_text_mode = text_mode;
                if matches!(text_mode, BlockTextMode::SwapFgBg)
                    && contrast_ratio(cell_background, color) < 2.0
                {
                    effective_text_mode = BlockTextMode::ForceContrast;
                    fallback_reason = Some("swap_fg_bg_low_contrast");
                }
                resolved_text_mode = effective_text_mode;
                let fill_text = match effective_text_mode {
                    BlockTextMode::SwapFgBg => cell_background,
                    BlockTextMode::ForceContrast => pick_contrast_text_color(color),
                };
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
                        pixels[idx] = color.0;
                        pixels[idx + 1] = color.1;
                        pixels[idx + 2] = color.2;
                        pixels[idx + 3] = 255;
                    }
                }
                let inset_x = (cell_w / 8).max(1);
                let inset_y = (cell_h / 8).max(1);
                if cell_w > inset_x.saturating_mul(2) && cell_h > inset_y.saturating_mul(2) {
                    for py in inset_y..(cell_h - inset_y) {
                        let y = y0 + py;
                        if y >= frame_height {
                            continue;
                        }
                        for px in inset_x..(cell_w - inset_x) {
                            let x = x0 + px;
                            if x >= frame_width {
                                continue;
                            }
                            let idx = (y * frame_width + x) * 4;
                            pixels[idx] = fill_text.0;
                            pixels[idx + 1] = fill_text.1;
                            pixels[idx + 2] = fill_text.2;
                            pixels[idx + 3] = 255;
                        }
                    }
                }
            }
            BlockPaintMode::Outline => {
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
                        if px > 0
                            && py > 0
                            && px < cell_w.saturating_sub(1)
                            && py < cell_h.saturating_sub(1)
                        {
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
        },
        CursorVisualShape::Bar => {
            let bar_width =
                ((cell_w.saturating_mul(usize::from(bar_width_pct.clamp(1, 100)))) / 100).max(1);
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
            let line_height =
                ((cell_h.saturating_mul(usize::from(underline_height_pct.clamp(1, 100)))) / 100)
                    .max(1);
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
    let _ = cell_foreground;
    (resolved_paint_mode, resolved_text_mode, fallback_reason)
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn export_recording_gif(
    events: &[DisplayTrackEnvelope],
    output: &str,
    speed: f64,
    fps: u32,
    max_duration: Option<u64>,
    max_frames: Option<u32>,
    terminal_profile: Option<&terminal_profile::DetectedTerminalProfile>,
    recording_profile: Option<&terminal_profile::DetectedTerminalProfile>,
    host_profile: Option<&terminal_profile::DetectedTerminalProfile>,
    renderer: RecordingRenderMode,
    cell_size: Option<(u16, u16)>,
    cell_width: Option<u16>,
    cell_height: Option<u16>,
    font_family: Option<&str>,
    font_size: Option<f32>,
    line_height: Option<f32>,
    font_path: &[String],
    palette_source: RecordingPaletteSource,
    palette_foreground: Option<&str>,
    palette_background: Option<&str>,
    palette_color: &[String],
    cursor_mode: RecordingCursorMode,
    cursor_shape: RecordingCursorShape,
    cursor_blink: RecordingCursorBlinkMode,
    cursor_blink_period_ms: u32,
    cursor_color: &str,
    cursor_profile: RecordingCursorProfile,
    cursor_solid_after_activity_ms: Option<u32>,
    cursor_solid_after_input_ms: Option<u32>,
    cursor_solid_after_output_ms: Option<u32>,
    cursor_solid_after_cursor_ms: Option<u32>,
    cursor_paint_mode: RecordingCursorPaintMode,
    cursor_text_mode: RecordingCursorTextMode,
    cursor_bar_width_pct: u8,
    cursor_underline_height_pct: u8,
    export_metadata: Option<&str>,
    show_progress: bool,
) -> Result<()> {
    let mut profiler = ExportProfiler::new();
    let speed = if speed <= 0.0 { 1.0 } else { speed };
    let fps = fps.max(1);
    let frame_interval_ns = (1_000_000_000_f64 / f64::from(fps)) as u64;
    let estimate = estimate_export_progress(events, speed, fps, max_duration, max_frames);
    let mut progress = ExportProgress::new(show_progress, estimate);
    let profile_defaults = terminal_profile.map(|profile| &profile.cursor_defaults);
    let resolved_shape = if matches!(cursor_shape, RecordingCursorShape::Auto) {
        profile_defaults
            .and_then(|defaults| defaults.shape)
            .map_or(cursor_shape, |shape| match shape {
                terminal_profile::CursorDefaultShape::Block => RecordingCursorShape::Block,
                terminal_profile::CursorDefaultShape::Bar => RecordingCursorShape::Bar,
                terminal_profile::CursorDefaultShape::Underline => RecordingCursorShape::Underline,
            })
    } else {
        cursor_shape
    };
    let resolved_blink = if matches!(cursor_blink, RecordingCursorBlinkMode::Auto) {
        profile_defaults
            .and_then(|defaults| defaults.blink)
            .map_or(cursor_blink, |blink| match blink {
                terminal_profile::CursorDefaultBlink::On => RecordingCursorBlinkMode::On,
                terminal_profile::CursorDefaultBlink::Off => RecordingCursorBlinkMode::Off,
            })
    } else {
        cursor_blink
    };
    let resolved_profile = if matches!(cursor_profile, RecordingCursorProfile::Auto) {
        profile_defaults
            .and_then(|defaults| defaults.profile)
            .map_or(RecordingCursorProfile::Generic, |profile| match profile {
                terminal_profile::CursorDefaultProfile::Ghostty => RecordingCursorProfile::Ghostty,
                terminal_profile::CursorDefaultProfile::Generic => RecordingCursorProfile::Generic,
            })
    } else {
        cursor_profile
    };
    let resolved_paint_mode = if matches!(cursor_paint_mode, RecordingCursorPaintMode::Auto) {
        profile_defaults
            .and_then(|defaults| defaults.paint_mode)
            .map_or(
                match resolved_profile {
                    RecordingCursorProfile::Ghostty => RecordingCursorPaintMode::Fill,
                    _ => RecordingCursorPaintMode::Invert,
                },
                |mode| match mode {
                    terminal_profile::CursorDefaultPaintMode::Invert => {
                        RecordingCursorPaintMode::Invert
                    }
                    terminal_profile::CursorDefaultPaintMode::Fill => {
                        RecordingCursorPaintMode::Fill
                    }
                    terminal_profile::CursorDefaultPaintMode::Outline => {
                        RecordingCursorPaintMode::Outline
                    }
                },
            )
    } else {
        cursor_paint_mode
    };
    let resolved_text_mode = if matches!(cursor_text_mode, RecordingCursorTextMode::Auto) {
        profile_defaults
            .and_then(|defaults| defaults.text_mode)
            .map_or(
                match resolved_profile {
                    RecordingCursorProfile::Ghostty => RecordingCursorTextMode::SwapFgBg,
                    _ => RecordingCursorTextMode::ForceContrast,
                },
                |mode| match mode {
                    terminal_profile::CursorDefaultTextMode::SwapFgBg => {
                        RecordingCursorTextMode::SwapFgBg
                    }
                    terminal_profile::CursorDefaultTextMode::ForceContrast => {
                        RecordingCursorTextMode::ForceContrast
                    }
                },
            )
    } else {
        cursor_text_mode
    };
    let resolved_bar_width_pct = profile_defaults
        .and_then(|defaults| defaults.bar_width_pct)
        .unwrap_or(cursor_bar_width_pct)
        .clamp(1, 100);
    let resolved_underline_height_pct = profile_defaults
        .and_then(|defaults| defaults.underline_height_pct)
        .unwrap_or(cursor_underline_height_pct)
        .clamp(1, 100);
    let resolved_solid_after_input_ms = cursor_solid_after_input_ms
        .or(cursor_solid_after_activity_ms)
        .or_else(|| profile_defaults.and_then(|defaults| defaults.solid_after_input_ms))
        .unwrap_or(500);
    let resolved_solid_after_output_ms = cursor_solid_after_output_ms
        .or(cursor_solid_after_activity_ms)
        .or_else(|| profile_defaults.and_then(|defaults| defaults.solid_after_output_ms))
        .unwrap_or(500);
    let resolved_solid_after_cursor_ms = cursor_solid_after_cursor_ms
        .or(cursor_solid_after_activity_ms)
        .or_else(|| profile_defaults.and_then(|defaults| defaults.solid_after_cursor_ms))
        .unwrap_or(500);
    let color_input = cursor_color.trim();
    let (resolved_color_label, resolved_color_override) =
        if color_input.is_empty() || color_input.eq_ignore_ascii_case("auto") {
            profile_defaults
                .and_then(|defaults| defaults.color.as_deref())
                .map_or_else(
                    || ("auto".to_string(), None),
                    |profile_color| {
                        let parsed = parse_cursor_color(profile_color).ok().flatten();
                        if parsed.is_some() {
                            (profile_color.to_string(), parsed)
                        } else {
                            ("auto".to_string(), None)
                        }
                    },
                )
        } else {
            (color_input.to_string(), parse_cursor_color(color_input)?)
        };

    let cursor_options = CursorExportOptions {
        mode: cursor_mode,
        shape: resolved_shape,
        blink: resolved_blink,
        profile: resolved_profile,
        blink_period_ns: u64::from(cursor_blink_period_ms.max(1)).saturating_mul(1_000_000),
        solid_after_input_ns: u64::from(resolved_solid_after_input_ms).saturating_mul(1_000_000),
        solid_after_output_ns: u64::from(resolved_solid_after_output_ms).saturating_mul(1_000_000),
        solid_after_cursor_ns: u64::from(resolved_solid_after_cursor_ms).saturating_mul(1_000_000),
        paint_mode: resolved_paint_mode,
        text_mode: resolved_text_mode,
        bar_width_pct: resolved_bar_width_pct,
        underline_height_pct: resolved_underline_height_pct,
        color_label: resolved_color_label,
        color_override: resolved_color_override,
    };

    let (max_cols, max_rows) = infer_export_terminal_bounds(events)?;

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
    let renderer_init_started_at = profiler.stage_started();
    let palette = resolve_export_palette(
        palette_source,
        recording_profile,
        host_profile,
        palette_foreground,
        palette_background,
        palette_color,
    )?;
    let mut glyph_renderer = match render_options.mode {
        RecordingRenderMode::Font => GlyphRenderer::new(cell_w, cell_h, &render_options),
        RecordingRenderMode::Bitmap => None,
    };
    let mut resvg_renderer = match render_options.mode {
        RecordingRenderMode::Font => Some(
            ResvgFrameRenderer::new(max_rows, max_cols, cell_w, cell_h, &render_options)
                .map_err(|error| {
                    profiler.note_resvg_fallback();
                    tracing::warn!(
                        "recording export: resvg renderer init failed, falling back to bitmap: {error:#}"
                    );
                    error
                })
                .ok(),
        ),
        RecordingRenderMode::Bitmap => None,
    }
    .flatten();
    let mut bitmap_cache = BitmapGlyphCache::new(usize::from(cell_w), usize::from(cell_h));
    profiler.record_renderer_init(renderer_init_started_at);

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

    let mut terminal_grid = bmux_terminal_grid::TerminalGridStream::new(
        max_cols.max(1),
        max_rows.max(1),
        bmux_terminal_grid::GridLimits::default(),
    )
    .expect("recording export grid dimensions are valid");
    let mut current_cols = max_cols;
    let mut current_rows = max_rows;
    let mut emitted_frames = 0_u32;
    let mut processed_frame_events = 0_u32;
    let mut previous_emit_frame_idx = None::<u32>;
    let mut gif_delay_clock = GifDelayClock::new(fps);
    let mut cursor_state = CursorReplayState::default();
    let mut snapshot_cursor_state = None::<RecordedCursorSnapshot>;
    let mut cursor_frames = export_metadata.map(|_| Vec::<ExportCursorFrame>::new());
    let mut blink_anchor_ns = None::<u64>;
    let mut last_input_activity_ns = None::<u64>;
    let mut last_output_activity_ns = None::<u64>;
    let mut last_cursor_activity_ns = None::<u64>;
    let mut warned_cursor_snapshot_fallback = false;
    let mut previous_visual_state = None::<FrameVisualState>;
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
        profiler.record_frame_considered();
        let frame_time_ns = u64::from(frame_idx).saturating_mul(frame_interval_ns);
        let apply_started_at = profiler.stage_started();
        let mut frame_had_display_change = false;
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
                    let _ = terminal_grid.resize(current_cols, current_rows);
                    frame_had_display_change = true;
                }
                DisplayTrackEvent::FrameBytes { data } => {
                    update_cursor_replay_state(&mut cursor_state, data);
                    terminal_grid.process(data);
                    processed_frame_events = processed_frame_events.saturating_add(1);
                    frame_had_display_change = true;
                }
                DisplayTrackEvent::CursorSnapshot {
                    x,
                    y,
                    visible,
                    shape,
                    blink_enabled,
                } => {
                    snapshot_cursor_state = Some(RecordedCursorSnapshot {
                        x: *x,
                        y: *y,
                        visible: *visible,
                        shape: *shape,
                        blink_enabled: *blink_enabled,
                    });
                    frame_had_display_change = true;
                }
                DisplayTrackEvent::Activity { kind } => match kind {
                    DisplayActivityKind::Input => {
                        last_input_activity_ns = Some(scaled_ns);
                    }
                    DisplayActivityKind::Output => {
                        last_output_activity_ns = Some(scaled_ns);
                    }
                    DisplayActivityKind::Cursor => {
                        last_cursor_activity_ns = Some(scaled_ns);
                    }
                },
                DisplayTrackEvent::StreamOpened { .. } | DisplayTrackEvent::StreamClosed => {}
                DisplayTrackEvent::ImageUpdate { .. } => {
                    frame_had_display_change = true;
                }
            }
            event_index = event_index.saturating_add(1);
        }
        profiler.record_apply_events(apply_started_at);

        if processed_frame_events == 0 {
            progress.update(processed_frame_events, emitted_frames, false);
            continue;
        }

        let (snapshot, cursor_source) = snapshot_cursor_state.map_or_else(
            || {
                if !warned_cursor_snapshot_fallback {
                    tracing::warn!(
                        "recording export: display track missing initial cursor snapshot; using structured grid cursor fallback until snapshots appear"
                    );
                    warned_cursor_snapshot_fallback = true;
                }
                (
                    cursor_snapshot_from_grid_fallback(terminal_grid.grid(), cursor_state),
                    "grid_fallback",
                )
            },
            |snapshot| (snapshot, "snapshot"),
        );
        let cursor_row = snapshot.y;
        let cursor_col = snapshot.x;
        let grid_cursor_visible = snapshot.visible;
        let shape = effective_cursor_shape(&cursor_options, cursor_state, snapshot.shape);
        let (cursor_visible, blink_on, visible_reason) = compute_cursor_visibility(
            &cursor_options,
            cursor_state,
            snapshot.blink_enabled,
            grid_cursor_visible,
            frame_time_ns,
            last_input_activity_ns,
            last_output_activity_ns,
            last_cursor_activity_ns,
            &mut blink_anchor_ns,
        );
        let visual_state = FrameVisualState {
            rows: current_rows,
            cols: current_cols,
            cursor_row,
            cursor_col,
            cursor_visible,
            shape,
            blink_on,
        };
        if !frame_had_display_change && previous_visual_state == Some(visual_state) {
            profiler.record_frame_skipped();
            progress.update(processed_frame_events, emitted_frames, false);
            continue;
        }

        let frame_span =
            previous_emit_frame_idx.map_or(1, |previous| frame_idx.saturating_sub(previous).max(1));
        let delay_cs = gif_delay_clock.delay_for_frame_span(frame_span);
        let render_started_at = profiler.stage_started();
        let mut pixels = if render_options.mode == RecordingRenderMode::Font {
            if let Some(renderer) = resvg_renderer.as_mut() {
                match renderer.render(terminal_grid.grid(), current_rows, current_cols, &palette) {
                    Ok(pixels) => pixels,
                    Err(error) => {
                        profiler.note_resvg_fallback();
                        tracing::warn!(
                            "recording export: resvg frame render failed, falling back to bitmap: {error:#}"
                        );
                        resvg_renderer = None;
                        render_screen_rgba(
                            terminal_grid.grid(),
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
                    }
                }
            } else {
                render_screen_rgba(
                    terminal_grid.grid(),
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
            }
        } else {
            render_screen_rgba(
                terminal_grid.grid(),
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

        if cursor_visible && cursor_row < current_rows && cursor_col < current_cols {
            let (cell_foreground, cell_background) = grid_cell_at(
                terminal_grid.grid(),
                usize::from(cursor_row),
                usize::from(cursor_col),
            )
            .map_or(((255, 255, 255), (0, 0, 0)), |cell| {
                resolved_grid_cell_colors(terminal_grid.grid(), &cell, &palette)
            });
            let cursor_color_rgb = cursor_options.color_override.unwrap_or(cell_foreground);
            let (paint_mode_used, text_mode_used, paint_fallback_reason) = overlay_cursor_rgba(
                &mut pixels,
                usize::from(width),
                usize::from(height),
                usize::from(cell_w),
                usize::from(cell_h),
                cursor_row,
                cursor_col,
                shape,
                match cursor_options.paint_mode {
                    RecordingCursorPaintMode::Auto | RecordingCursorPaintMode::Invert => {
                        BlockPaintMode::Invert
                    }
                    RecordingCursorPaintMode::Fill => BlockPaintMode::Fill,
                    RecordingCursorPaintMode::Outline => BlockPaintMode::Outline,
                },
                match cursor_options.text_mode {
                    RecordingCursorTextMode::Auto | RecordingCursorTextMode::SwapFgBg => {
                        BlockTextMode::SwapFgBg
                    }
                    RecordingCursorTextMode::ForceContrast => BlockTextMode::ForceContrast,
                },
                cursor_options.bar_width_pct,
                cursor_options.underline_height_pct,
                cell_foreground,
                cell_background,
                cursor_color_rgb,
            );
            if let Some(frames) = cursor_frames.as_mut() {
                frames.push(ExportCursorFrame {
                    mono_ns: frame_time_ns,
                    row: cursor_row,
                    col: cursor_col,
                    visible: cursor_visible,
                    shape: cursor_shape_name(shape),
                    blink_on,
                    cursor_source,
                    visible_reason,
                    paint_mode_used: paint_mode_name(paint_mode_used),
                    text_mode_used: text_mode_name(text_mode_used),
                    paint_fallback_reason,
                    last_input_activity_ns,
                    last_output_activity_ns,
                    last_cursor_activity_ns,
                });
            }
        } else if let Some(frames) = cursor_frames.as_mut() {
            frames.push(ExportCursorFrame {
                mono_ns: frame_time_ns,
                row: cursor_row,
                col: cursor_col,
                visible: cursor_visible,
                shape: cursor_shape_name(shape),
                blink_on,
                cursor_source,
                visible_reason,
                paint_mode_used: paint_mode_name(match cursor_options.paint_mode {
                    RecordingCursorPaintMode::Auto | RecordingCursorPaintMode::Invert => {
                        BlockPaintMode::Invert
                    }
                    RecordingCursorPaintMode::Fill => BlockPaintMode::Fill,
                    RecordingCursorPaintMode::Outline => BlockPaintMode::Outline,
                }),
                text_mode_used: text_mode_name(match cursor_options.text_mode {
                    RecordingCursorTextMode::Auto | RecordingCursorTextMode::SwapFgBg => {
                        BlockTextMode::SwapFgBg
                    }
                    RecordingCursorTextMode::ForceContrast => BlockTextMode::ForceContrast,
                }),
                paint_fallback_reason: None,
                last_input_activity_ns,
                last_output_activity_ns,
                last_cursor_activity_ns,
            });
        }
        profiler.record_render(render_started_at);
        let encode_started_at = profiler.stage_started();
        let mut frame =
            GifFrame::from_rgba_speed(width, height, &mut pixels, GIF_QUANTIZATION_SAMPLE_FACTOR);
        frame.delay = delay_cs;
        encoder
            .write_frame(&frame)
            .context("failed writing gif frame")?;
        profiler.record_encode(encode_started_at);
        previous_visual_state = Some(visual_state);
        previous_emit_frame_idx = Some(frame_idx);
        emitted_frames = emitted_frames.saturating_add(1);
        profiler.record_frame_emitted();
        progress.update(processed_frame_events, emitted_frames, false);
    }

    progress.finish(processed_frame_events, emitted_frames);
    profiler.finish(processed_frame_events, emitted_frames);

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
                profile: match cursor_options.profile {
                    RecordingCursorProfile::Auto => "auto",
                    RecordingCursorProfile::Ghostty => "ghostty",
                    RecordingCursorProfile::Generic => "generic",
                },
                blink_period_ms: cursor_blink_period_ms.max(1),
                solid_after_input_ms: resolved_solid_after_input_ms,
                solid_after_output_ms: resolved_solid_after_output_ms,
                solid_after_cursor_ms: resolved_solid_after_cursor_ms,
                paint_mode: match cursor_options.paint_mode {
                    RecordingCursorPaintMode::Auto => "auto",
                    RecordingCursorPaintMode::Invert => "invert",
                    RecordingCursorPaintMode::Fill => "fill",
                    RecordingCursorPaintMode::Outline => "outline",
                },
                text_mode: match cursor_options.text_mode {
                    RecordingCursorTextMode::Auto => "auto",
                    RecordingCursorTextMode::SwapFgBg => "swap_fg_bg",
                    RecordingCursorTextMode::ForceContrast => "force_contrast",
                },
                bar_width_pct: cursor_options.bar_width_pct,
                underline_height_pct: cursor_options.underline_height_pct,
                color: &cursor_options.color_label,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameVisualState {
    rows: u16,
    cols: u16,
    cursor_row: u16,
    cursor_col: u16,
    cursor_visible: bool,
    shape: CursorVisualShape,
    blink_on: bool,
}

#[derive(Debug)]
struct GifDelayClock {
    fps: u32,
    remainder: u64,
}

impl GifDelayClock {
    fn new(fps: u32) -> Self {
        Self {
            fps: fps.max(1),
            remainder: 0,
        }
    }

    fn delay_for_frame_span(&mut self, frame_span: u32) -> u16 {
        let total = u64::from(frame_span.max(1))
            .saturating_mul(100)
            .saturating_add(self.remainder);
        let fps = u64::from(self.fps.max(1));
        let mut delay = total / fps;
        self.remainder = total % fps;
        if delay == 0 {
            delay = 1;
            self.remainder = 0;
        }
        u16::try_from(delay.min(u64::from(u16::MAX))).unwrap_or(u16::MAX)
    }
}

#[derive(Debug)]
struct ExportProfiler {
    enabled: bool,
    started_at: Instant,
    renderer_init: std::time::Duration,
    apply_events: std::time::Duration,
    render: std::time::Duration,
    encode: std::time::Duration,
    frames_considered: u32,
    frames_emitted: u32,
    frames_skipped: u32,
    resvg_fallbacks: u32,
}

impl ExportProfiler {
    fn new() -> Self {
        let enabled = std::env::var("BMUX_RECORDING_EXPORT_PROFILE")
            .ok()
            .is_some_and(|value| {
                let normalized = value.trim().to_ascii_lowercase();
                matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
            });
        Self {
            enabled,
            started_at: Instant::now(),
            renderer_init: std::time::Duration::ZERO,
            apply_events: std::time::Duration::ZERO,
            render: std::time::Duration::ZERO,
            encode: std::time::Duration::ZERO,
            frames_considered: 0,
            frames_emitted: 0,
            frames_skipped: 0,
            resvg_fallbacks: 0,
        }
    }

    fn stage_started(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    fn record_renderer_init(&mut self, started_at: Option<Instant>) {
        if let Some(started_at) = started_at {
            self.renderer_init += started_at.elapsed();
        }
    }

    fn record_apply_events(&mut self, started_at: Option<Instant>) {
        if let Some(started_at) = started_at {
            self.apply_events += started_at.elapsed();
        }
    }

    fn record_render(&mut self, started_at: Option<Instant>) {
        if let Some(started_at) = started_at {
            self.render += started_at.elapsed();
        }
    }

    fn record_encode(&mut self, started_at: Option<Instant>) {
        if let Some(started_at) = started_at {
            self.encode += started_at.elapsed();
        }
    }

    const fn record_frame_considered(&mut self) {
        self.frames_considered = self.frames_considered.saturating_add(1);
    }

    const fn record_frame_emitted(&mut self) {
        self.frames_emitted = self.frames_emitted.saturating_add(1);
    }

    const fn record_frame_skipped(&mut self) {
        self.frames_skipped = self.frames_skipped.saturating_add(1);
    }

    const fn note_resvg_fallback(&mut self) {
        self.resvg_fallbacks = self.resvg_fallbacks.saturating_add(1);
    }

    fn finish(&self, processed_frame_events: u32, emitted_frames: u32) {
        if !self.enabled {
            return;
        }
        let elapsed = self.started_at.elapsed();
        let considered = self.frames_considered.max(1);
        let avg_render_ms = self.render.as_secs_f64() * 1000.0 / f64::from(considered);
        let avg_encode_ms = self.encode.as_secs_f64() * 1000.0 / f64::from(considered);
        tracing::info!(
            "recording export profile: elapsed={} init={} apply={} render={} encode={} frames_considered={} frames_emitted={} frames_skipped={} processed_frame_events={} emitted_frames={} resvg_fallbacks={} avg_render_ms={avg_render_ms:.3} avg_encode_ms={avg_encode_ms:.3}",
            format_duration_compact(elapsed),
            format_duration_compact(self.renderer_init),
            format_duration_compact(self.apply_events),
            format_duration_compact(self.render),
            format_duration_compact(self.encode),
            self.frames_considered,
            self.frames_emitted,
            self.frames_skipped,
            processed_frame_events,
            emitted_frames,
            self.resvg_fallbacks,
        );
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
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
    #[allow(clippy::cast_possible_truncation)]
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

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn render_screen_rgba(
    grid: &bmux_terminal_grid::TerminalGrid,
    rows: u16,
    cols: u16,
    max_rows: u16,
    max_cols: u16,
    cell_w: u16,
    cell_h: u16,
    palette: &ExportPalette,
    mut glyph_renderer: Option<&mut GlyphRenderer>,
    bitmap_cache: &mut BitmapGlyphCache,
) -> Vec<u8> {
    let width = usize::from(max_cols.saturating_mul(cell_w));
    let height = usize::from(max_rows.saturating_mul(cell_h));
    let mut pixels = vec![0_u8; width.saturating_mul(height).saturating_mul(4)];
    let cw = usize::from(cell_w);
    let cell_height_px = usize::from(cell_h);

    let display_rows = grid.display_rows(0, usize::from(rows));
    for row in 0..rows {
        for col in 0..cols {
            let cell = display_rows
                .get(usize::from(row))
                .and_then(|grid_row| grid_row.cells().get(usize::from(col)));
            let ((fg_r, fg_g, fg_b), (bg_r, bg_g, bg_b)) = cell.map_or_else(
                || {
                    (
                        resolve_grid_color(None, true, palette),
                        resolve_grid_color(None, false, palette),
                    )
                },
                |cell| resolved_grid_cell_colors(grid, cell, palette),
            );
            let x0 = usize::from(col).saturating_mul(cw);
            let y0 = usize::from(row).saturating_mul(cell_height_px);
            for py in 0..cell_height_px {
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

            let glyph_char = cell
                .filter(|cell| !cell.is_wide_continuation())
                .and_then(|cell| cell.text().chars().next())
                .unwrap_or(' ');
            if glyph_char == ' ' {
                continue;
            }

            let drawn_with_font = glyph_renderer.as_deref_mut().is_some_and(|renderer| {
                renderer.draw_cell(
                    &mut pixels,
                    width,
                    height,
                    x0,
                    y0,
                    glyph_char,
                    (fg_r, fg_g, fg_b),
                    (bg_r, bg_g, bg_b),
                )
            });
            if !drawn_with_font {
                draw_bitmap_glyph_rgba(
                    &mut pixels,
                    width,
                    height,
                    x0,
                    y0,
                    cw,
                    cell_height_px,
                    glyph_char,
                    (fg_r, fg_g, fg_b),
                    bitmap_cache,
                );
            }
        }
    }

    pixels
}

struct ResvgFrameRenderer {
    width: usize,
    height: usize,
    width_u32: u32,
    height_u32: u32,
    cell_width_px: usize,
    cell_height_px: usize,
    background_opacity: f32,
    backdrop_rgb: (u8, u8, u8),
    top_to_baseline: f32,
    font_size: f32,
    font_family_attr: String,
    options_usvg: usvg::Options<'static>,
    svg: String,
}

impl ResvgFrameRenderer {
    fn new(
        max_rows: u16,
        max_cols: u16,
        cell_w: u16,
        cell_h: u16,
        options: &RenderOptions,
    ) -> Result<Self> {
        let width = usize::from(max_cols.saturating_mul(cell_w));
        let height = usize::from(max_rows.saturating_mul(cell_h));
        let width_u32 = u32::try_from(width).context("render width exceeds u32")?;
        let height_u32 = u32::try_from(height).context("render height exceeds u32")?;
        let cell_width_px = usize::from(cell_w);
        let cell_height_px = usize::from(cell_h);
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
            .unwrap_or_else(|| (f32::from(cell_h) * 0.9).max(8.0));
        let top_to_baseline = metrics
            .as_ref()
            .map_or_else(|| f32::from(cell_h) * 0.8, |value| value.top_to_baseline_px);
        let font_family_attr = svg_font_family_list(&families);

        let font_family = families
            .first()
            .cloned()
            .unwrap_or_else(|| "monospace".to_string());
        let mut options_usvg = usvg::Options {
            font_family,
            font_size,
            ..usvg::Options::default()
        };
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

        Ok(Self {
            width,
            height,
            width_u32,
            height_u32,
            cell_width_px,
            cell_height_px,
            background_opacity: options.background_opacity,
            backdrop_rgb: options.backdrop_rgb,
            top_to_baseline,
            font_size,
            font_family_attr,
            options_usvg,
            svg: String::with_capacity(width.saturating_mul(height / 4).max(1024)),
        })
    }

    #[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
    fn render(
        &mut self,
        grid: &bmux_terminal_grid::TerminalGrid,
        rows: u16,
        cols: u16,
        palette: &ExportPalette,
    ) -> Result<Vec<u8>> {
        self.svg.clear();
        write!(
            &mut self.svg,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\">",
            self.width, self.height, self.width, self.height
        )
        .expect("svg write cannot fail");
        write!(
            &mut self.svg,
            "<g font-family=\"{}\" font-size=\"{:.3}\" text-rendering=\"optimizeLegibility\" dominant-baseline=\"alphabetic\" font-kerning=\"none\" font-variant-ligatures=\"none\">",
            xml_escape_attr(&self.font_family_attr),
            self.font_size
        )
        .expect("svg write cannot fail");

        let display_rows = grid.display_rows(0, usize::from(rows));
        for row in 0..rows {
            let mut row_runs = Vec::<TextRun>::new();
            let mut current_run = None::<TextRun>;
            for col in 0..cols {
                let cell = display_rows
                    .get(usize::from(row))
                    .and_then(|grid_row| grid_row.cells().get(usize::from(col)));
                let grid_style = cell
                    .map(|cell| grid.palette().get(cell.style()))
                    .unwrap_or_default();
                let (mut fg_rgb, bg_rgb) = cell.map_or_else(
                    || {
                        (
                            resolve_grid_color(None, true, palette),
                            resolve_grid_color(None, false, palette),
                        )
                    },
                    |cell| resolved_grid_cell_colors(grid, cell, palette),
                );
                if grid_style.dim {
                    fg_rgb = dim_rgb(fg_rgb);
                }
                let bg_rgb =
                    composite_with_backdrop(bg_rgb, self.background_opacity, self.backdrop_rgb);
                let x0 = usize::from(col).saturating_mul(self.cell_width_px);
                let y0 = usize::from(row).saturating_mul(self.cell_height_px);
                write!(
                    &mut self.svg,
                    "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"rgb({},{},{})\"/>",
                    x0, y0, self.cell_width_px, self.cell_height_px, bg_rgb.0, bg_rgb.1, bg_rgb.2
                )
                .expect("svg write cannot fail");

                let cell_text = cell
                    .filter(|cell| !cell.is_wide_continuation() && !cell.text().is_empty())
                    .map_or(" ", bmux_terminal_grid::Cell::text);
                let style = TextStyle {
                    fg_rgb,
                    bold: grid_style.bold,
                    italic: grid_style.italic,
                    underline: grid_style.underline,
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
                let x0 = usize::from(run.start_col).saturating_mul(self.cell_width_px);
                let y0 = usize::from(row).saturating_mul(self.cell_height_px);
                let text_y = y0 as f32 + self.top_to_baseline;
                let style_attrs = svg_style_attrs(run.style);
                let text_length = usize::from(run.cell_count).saturating_mul(self.cell_width_px);
                write!(
                    &mut self.svg,
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

        self.svg.push_str("</g></svg>");

        let tree = usvg::Tree::from_str(&self.svg, &self.options_usvg)
            .context("failed to parse SVG frame")?;
        let mut pixmap = tiny_skia::Pixmap::new(self.width_u32, self.height_u32)
            .ok_or_else(|| anyhow::anyhow!("failed to allocate pixmap for SVG frame"))?;
        resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
        Ok(pixmap.take())
    }
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

fn grid_cell_at(
    grid: &bmux_terminal_grid::TerminalGrid,
    row: usize,
    col: usize,
) -> Option<bmux_terminal_grid::Cell> {
    grid.display_rows(0, grid.height())
        .get(row)
        .and_then(|grid_row| grid_row.cells().get(col))
        .filter(|cell| !cell.is_wide_continuation())
        .cloned()
}

fn resolved_grid_cell_colors(
    grid: &bmux_terminal_grid::TerminalGrid,
    cell: &bmux_terminal_grid::Cell,
    palette: &ExportPalette,
) -> ((u8, u8, u8), (u8, u8, u8)) {
    let style = grid.palette().get(cell.style());
    let mut fg = resolve_grid_color(style.fg, true, palette);
    let mut bg = resolve_grid_color(style.bg, false, palette);
    if style.inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

fn svg_style_attrs(style: TextStyle) -> String {
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

#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
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
        let base_font_size = options
            .font_size_px
            .unwrap_or_else(|| f32::from(cell_h).max(8.0));
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

    #[allow(clippy::too_many_arguments, clippy::cast_precision_loss)]
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

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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

#[derive(Debug, Clone)]
struct ExportPalette {
    colors: [(u8, u8, u8); 256],
    default_fg: (u8, u8, u8),
    default_bg: (u8, u8, u8),
}

type PaletteRgb = (u8, u8, u8);
type PaletteOverride = (u8, PaletteRgb);

impl ExportPalette {
    fn xterm() -> Self {
        let colors = xterm_256_palette();
        Self {
            colors,
            default_fg: colors[15],
            default_bg: colors[0],
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ResolvedPaletteSource {
    Recording,
    Terminal,
    Xterm,
}

fn resolve_export_palette(
    source: RecordingPaletteSource,
    recording_profile: Option<&terminal_profile::DetectedTerminalProfile>,
    host_profile: Option<&terminal_profile::DetectedTerminalProfile>,
    palette_foreground: Option<&str>,
    palette_background: Option<&str>,
    palette_color: &[String],
) -> Result<ExportPalette> {
    let mut palette = ExportPalette::xterm();
    let resolved_source = match source {
        RecordingPaletteSource::Auto => {
            if recording_profile.is_some_and(profile_has_palette_data) {
                ResolvedPaletteSource::Recording
            } else if host_profile.is_some_and(profile_has_palette_data) {
                ResolvedPaletteSource::Terminal
            } else {
                ResolvedPaletteSource::Xterm
            }
        }
        RecordingPaletteSource::Recording => {
            if recording_profile.is_some_and(profile_has_palette_data) {
                ResolvedPaletteSource::Recording
            } else {
                ResolvedPaletteSource::Xterm
            }
        }
        RecordingPaletteSource::Terminal => {
            if host_profile.is_some_and(profile_has_palette_data) {
                ResolvedPaletteSource::Terminal
            } else {
                ResolvedPaletteSource::Xterm
            }
        }
        RecordingPaletteSource::Xterm => ResolvedPaletteSource::Xterm,
    };

    match resolved_source {
        ResolvedPaletteSource::Recording => {
            if let Some(profile) = recording_profile {
                apply_profile_palette(&mut palette, profile);
            }
        }
        ResolvedPaletteSource::Terminal => {
            if let Some(profile) = host_profile {
                apply_profile_palette(&mut palette, profile);
            }
        }
        ResolvedPaletteSource::Xterm => {}
    }

    if let Some(fg) = parse_palette_default_override(palette_foreground, "palette foreground")? {
        palette.default_fg = fg;
    }
    if let Some(bg) = parse_palette_default_override(palette_background, "palette background")? {
        palette.default_bg = bg;
    }
    let overrides = parse_palette_color_overrides(palette_color)?;
    for (index, rgb) in overrides {
        palette.colors[usize::from(index)] = rgb;
    }

    Ok(palette)
}

const fn profile_has_palette_data(profile: &terminal_profile::DetectedTerminalProfile) -> bool {
    profile.palette_defaults.foreground.is_some()
        || profile.palette_defaults.background.is_some()
        || !profile.palette_defaults.colors.is_empty()
}

fn apply_profile_palette(
    palette: &mut ExportPalette,
    profile: &terminal_profile::DetectedTerminalProfile,
) {
    if let Some(raw) = profile.palette_defaults.foreground.as_deref() {
        if let Some(rgb) = parse_rgb_color(raw) {
            palette.default_fg = rgb;
        } else {
            tracing::warn!(
                "recording export: ignoring invalid terminal profile foreground color '{raw}'"
            );
        }
    }
    if let Some(raw) = profile.palette_defaults.background.as_deref() {
        if let Some(rgb) = parse_rgb_color(raw) {
            palette.default_bg = rgb;
        } else {
            tracing::warn!(
                "recording export: ignoring invalid terminal profile background color '{raw}'"
            );
        }
    }
    for entry in &profile.palette_defaults.colors {
        if let Some(rgb) = parse_rgb_color(&entry.color) {
            palette.colors[usize::from(entry.index)] = rgb;
        } else {
            tracing::warn!(
                "recording export: ignoring invalid terminal profile palette entry {}='{}'",
                entry.index,
                entry.color
            );
        }
    }
}

fn parse_palette_default_override(
    value: Option<&str>,
    field_name: &str,
) -> Result<Option<(u8, u8, u8)>> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    let Some(rgb) = parse_rgb_color(trimmed) else {
        anyhow::bail!("invalid {field_name} '{raw}'; expected auto or a color value")
    };
    Ok(Some(rgb))
}

fn parse_palette_color_overrides(values: &[String]) -> Result<Vec<PaletteOverride>> {
    values
        .iter()
        .map(|value| parse_palette_color_override(value))
        .collect()
}

fn parse_palette_color_override(value: &str) -> Result<PaletteOverride> {
    let (index_raw, color_raw) = value.split_once('=').ok_or_else(|| {
        anyhow::anyhow!("invalid palette override '{value}'; expected INDEX=COLOR")
    })?;
    let index = parse_palette_index(index_raw.trim())
        .ok_or_else(|| anyhow::anyhow!("invalid palette index '{index_raw}'; expected 0..255"))?;
    let color = color_raw.trim();
    let rgb = parse_rgb_color(color).ok_or_else(|| {
        anyhow::anyhow!("invalid palette color '{color_raw}'; expected a color value")
    })?;
    Ok((index, rgb))
}

fn parse_palette_index(value: &str) -> Option<u8> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ] {
        if let Some(digits) = trimmed.strip_prefix(prefix) {
            if digits.is_empty() {
                return None;
            }
            let parsed = u16::from_str_radix(digits, radix).ok()?;
            return u8::try_from(parsed).ok();
        }
    }
    let parsed = trimmed.parse::<u16>().ok()?;
    u8::try_from(parsed).ok()
}

fn resolve_grid_color(
    color: Option<bmux_terminal_grid::Color>,
    foreground: bool,
    palette: &ExportPalette,
) -> (u8, u8, u8) {
    match color {
        None => {
            if foreground {
                palette.default_fg
            } else {
                palette.default_bg
            }
        }
        Some(bmux_terminal_grid::Color::Indexed(idx)) => palette.colors[usize::from(idx)],
        Some(bmux_terminal_grid::Color::Rgb { r, g, b }) => (r, g, b),
    }
}

fn parse_rgb_color(value: &str) -> Option<(u8, u8, u8)> {
    parse_hex_rgb(value).or_else(|| parse_osc_rgb(value))
}

fn parse_hex_rgb(value: &str) -> Option<(u8, u8, u8)> {
    let trimmed = value.trim();
    let hex = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if hex.len() != 6 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

fn parse_osc_rgb(value: &str) -> Option<(u8, u8, u8)> {
    let trimmed = value.trim();
    let body = trimmed
        .strip_prefix("rgb:")
        .or_else(|| trimmed.strip_prefix("RGB:"))?;
    let mut channels = body.split('/');
    let r = channels.next().and_then(hex_component_to_u8)?;
    let g = channels.next().and_then(hex_component_to_u8)?;
    let b = channels.next().and_then(hex_component_to_u8)?;
    if channels.next().is_some() {
        return None;
    }
    Some((r, g, b))
}

fn hex_component_to_u8(value: &str) -> Option<u8> {
    if !(1..=4).contains(&value.len()) || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let parsed = u16::from_str_radix(value, 16).ok()?;
    let bits = u32::try_from(value.len()).ok()?.saturating_mul(4);
    let max = (1_u32 << bits).saturating_sub(1);
    if max == 0 {
        return None;
    }
    let scaled = (u32::from(parsed)
        .saturating_mul(255)
        .saturating_add(max / 2))
        / max;
    u8::try_from(scaled).ok()
}

fn xterm_256_palette() -> [(u8, u8, u8); 256] {
    let mut colors = [(0_u8, 0_u8, 0_u8); 256];
    let base = [
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
    colors[..16].copy_from_slice(&base);

    let steps = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
    let mut index = 16_usize;
    for r in steps {
        for g in steps {
            for b in steps {
                colors[index] = (r, g, b);
                index = index.saturating_add(1);
            }
        }
    }

    for i in 0..24_u8 {
        let value = 8 + i * 10;
        colors[index] = (value, value, value);
        index = index.saturating_add(1);
    }
    colors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_supported_manifest_reads_nested_summary_format_version() {
        let recording_dir = std::env::temp_dir().join(format!(
            "bmux-recording-plugin-export-test-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&recording_dir).expect("recording dir should be created");
        std::fs::write(
            recording_dir.join("manifest.json"),
            format!(r#"{{"summary":{{"format_version":{RECORDING_FORMAT_VERSION}}}}}"#),
        )
        .expect("manifest should be written");

        ensure_supported_manifest(&recording_dir)
            .expect("nested manifest summary should be accepted");

        std::fs::remove_dir_all(&recording_dir).expect("recording dir should be removed");
    }
}
