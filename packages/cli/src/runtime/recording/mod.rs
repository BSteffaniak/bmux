use super::*;
use ab_glyph::{Font, FontArc, PxScale, ScaleFont, point};
use std::collections::HashSet;

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

fn recording_event_kind_arg_to_ipc(kind: RecordingEventKindArg) -> RecordingEventKind {
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
    match connect_if_running(ConnectionPolicyScope::Normal, "bmux-cli-recording-delete").await? {
        Some(mut client) => {
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
        }
        None => {
            let recordings = list_recordings_from_disk()?;
            let resolved = resolve_recording_id_prefix(recording_id_or_prefix, &recordings)?;
            delete_recording_dir(resolved)?;
            println!("deleted recording {resolved}");
        }
    }
    Ok(0)
}

pub(super) async fn run_recording_delete_all(yes: bool) -> Result<u8> {
    if !confirm_delete_all_recordings(yes)? {
        println!("skipped recording delete-all");
        return Ok(0);
    }

    cleanup_stale_pid_file().await?;
    match connect_if_running(
        ConnectionPolicyScope::Normal,
        "bmux-cli-recording-delete-all",
    )
    .await?
    {
        Some(mut client) => {
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
        }
        None => {
            let deleted_count = delete_all_recordings_from_disk()?;
            println!("deleted {deleted_count} recordings");
        }
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
    Ok(if report.pass { 0 } else { 1 })
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

    match format {
        RecordingExportFormat::Gif => export_recording_gif(
            &events,
            output,
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
        )?,
    }

    println!(
        "export complete: format={:?} view_client={} output={}",
        format, selected_client, output
    );
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
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .with_context(|| format!("failed opening display track {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let event: DisplayTrackEnvelope = serde_json::from_str(&line)
            .with_context(|| format!("failed parsing display event in {}", path.display()))?;
        events.push(event);
    }
    Ok(events)
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
    let (window_width_px, window_height_px) = terminal::window_size()
        .ok()
        .map(|value| {
            (
                (value.width > 0).then_some(value.width),
                (value.height > 0).then_some(value.height),
            )
        })
        .unwrap_or((None, None));

    let (cell_width_px, cell_height_px) = terminal::size()
        .ok()
        .and_then(|(cols, rows)| {
            let window_width = window_width_px?;
            let window_height = window_height_px?;
            infer_cell_metrics(window_width, window_height, cols, rows)
        })
        .map(|value| (Some(value.width), Some(value.height)))
        .unwrap_or((None, None));

    (
        cell_width_px,
        cell_height_px,
        window_width_px,
        window_height_px,
    )
}

fn export_recording_gif(
    events: &[DisplayTrackEnvelope],
    output: &str,
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
) -> Result<()> {
    let speed = if speed <= 0.0 { 1.0 } else { speed };
    let fps = fps.max(1);
    let frame_interval_ns = (1_000_000_000_f64 / f64::from(fps)) as u64;

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
    let render_options =
        build_render_options(renderer, font_family, font_size, line_height, font_path)?;
    let palette = xterm_256_palette();
    let glyph_renderer = match render_options.mode {
        RecordingRenderMode::Font => GlyphRenderer::new(cell_w, cell_h, &render_options),
        RecordingRenderMode::Bitmap => None,
    };

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
    let mut previous_emit_ns = None::<u64>;
    let mut first_mono_ns = None::<u64>;

    for event in events {
        if first_mono_ns.is_none() {
            first_mono_ns = Some(event.mono_ns);
        }
        if let Some(limit_secs) = max_duration
            && let Some(start_ns) = first_mono_ns
            && event.mono_ns.saturating_sub(start_ns) / 1_000_000_000 > limit_secs
        {
            break;
        }
        if let Some(limit) = max_frames
            && emitted_frames >= limit
        {
            break;
        }

        match &event.event {
            DisplayTrackEvent::Resize { cols, rows } => {
                current_cols = (*cols).max(1);
                current_rows = (*rows).max(1);
                parser.screen_mut().set_size(current_rows, current_cols);
            }
            DisplayTrackEvent::FrameBytes { data } => {
                parser.process(data);
                let scaled_ns = (event.mono_ns as f64 / speed) as u64;
                let should_emit = previous_emit_ns
                    .is_none_or(|previous| scaled_ns.saturating_sub(previous) >= frame_interval_ns);
                if !should_emit {
                    continue;
                }
                let delay_cs = previous_emit_ns.map_or(1_u16, |previous| {
                    let delta_ns = scaled_ns.saturating_sub(previous);
                    ((delta_ns / 10_000_000).max(1).min(u64::from(u16::MAX))) as u16
                });
                let mut pixels = render_screen_rgba(
                    parser.screen(),
                    current_rows,
                    current_cols,
                    max_rows,
                    max_cols,
                    cell_w,
                    cell_h,
                    &palette,
                    glyph_renderer.as_ref(),
                );
                let mut frame = GifFrame::from_rgba_speed(width, height, &mut pixels, 1);
                frame.delay = delay_cs;
                encoder
                    .write_frame(&frame)
                    .context("failed writing gif frame")?;
                previous_emit_ns = Some(scaled_ns);
                emitted_frames = emitted_frames.saturating_add(1);
            }
            DisplayTrackEvent::StreamOpened { .. } | DisplayTrackEvent::StreamClosed => {}
        }
    }

    if emitted_frames == 0 {
        anyhow::bail!("no drawable frame events found in display track")
    }
    Ok(())
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
    glyph_renderer: Option<&GlyphRenderer>,
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
            if let Some(renderer) = glyph_renderer
                && !is_box_drawing_char(glyph_char)
            {
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
                );
            }
        }
    }

    pixels
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
) {
    let glyph = font8x8::BASIC_FONTS
        .get(glyph_char)
        .or_else(|| font8x8::BASIC_FONTS.get('?'))
        .unwrap_or([0_u8; 8]);
    for py in 0..cell_h {
        let y = y0 + py;
        if y >= height {
            continue;
        }
        let glyph_row = ((py.saturating_mul(8)) / cell_h).min(7);
        let bits = glyph[glyph_row];
        let row_start = y.saturating_mul(width);
        for px in 0..cell_w {
            let x = x0 + px;
            if x >= width {
                continue;
            }
            let glyph_col = ((px.saturating_mul(8)) / cell_w).min(7);
            if ((bits >> glyph_col) & 1) == 1 {
                let idx = (row_start + x).saturating_mul(4);
                pixels[idx] = fg_rgb.0;
                pixels[idx + 1] = fg_rgb.1;
                pixels[idx + 2] = fg_rgb.2;
                pixels[idx + 3] = 255;
            }
        }
    }
}

fn is_box_drawing_char(ch: char) -> bool {
    matches!(ch as u32, 0x2500..=0x257f | 0x2580..=0x259f)
}

struct RenderOptions {
    mode: RecordingRenderMode,
    font_families: Vec<String>,
    font_paths: Vec<String>,
    font_size_px: Option<f32>,
    line_height_mult: f32,
}

fn build_render_options(
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
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(RenderOptions {
        mode: renderer,
        font_families,
        font_paths: font_path.to_vec(),
        font_size_px: font_size,
        line_height_mult: line_height.unwrap_or(1.0),
    })
}

struct GlyphRenderer {
    font: FontArc,
    scale: PxScale,
    baseline_offset: f32,
}

impl GlyphRenderer {
    fn new(cell_w: u16, cell_h: u16, options: &RenderOptions) -> Option<Self> {
        let font = load_monospace_font(options)?;
        let base_font_size = options
            .font_size_px
            .unwrap_or((f32::from(cell_h) * 0.9).max(8.0));
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
        let glyph_height = scaled.height();
        let line_height = glyph_height * options.line_height_mult.max(0.5);
        let baseline_offset = ((f32::from(cell_h) - line_height) / 2.0).max(0.0) + scaled.ascent();
        Some(Self {
            font,
            scale,
            baseline_offset,
        })
    }

    fn draw_cell(
        &self,
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
        let glyph = self.font.glyph_id(glyph_char).with_scale_and_position(
            self.scale,
            point(x0 as f32, y0 as f32 + self.baseline_offset),
        );
        let Some(outlined) = self.font.outline_glyph(glyph) else {
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
            let alpha = if coverage >= 0.75 {
                1.0
            } else if coverage <= 0.1 {
                0.0
            } else {
                coverage
            };
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
    ((f32::from(fg) * alpha) + (f32::from(bg) * (1.0 - alpha))).round() as u8
}

fn load_monospace_font(options: &RenderOptions) -> Option<FontArc> {
    let mut candidates = Vec::<String>::new();
    candidates.extend(options.font_paths.iter().cloned());
    for family in &options.font_families {
        candidates.extend(
            font_family_candidates(family)
                .into_iter()
                .map(str::to_string),
        );
    }
    candidates.extend(default_font_candidates().into_iter().map(str::to_string));

    let mut seen = HashSet::<String>::new();
    for path in candidates {
        if !seen.insert(path.clone()) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if let Ok(font) = FontArc::try_from_vec(bytes) {
            return Some(font);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn default_font_candidates() -> Vec<&'static str> {
    vec![
        "/System/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/Monaco.ttf",
        "/System/Library/Fonts/SFNSMono.ttf",
        "/System/Library/Fonts/Supplemental/Courier New.ttf",
        "/System/Library/Fonts/Courier.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/liberation2/LiberationMono-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    ]
}

#[cfg(not(target_os = "macos"))]
fn default_font_candidates() -> Vec<&'static str> {
    vec![
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/liberation2/LiberationMono-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
    ]
}

fn font_family_candidates(family: &str) -> Vec<&'static str> {
    let normalized = family
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '-' && *ch != '_')
        .collect::<String>();
    match normalized.as_str() {
        "menlo" => vec!["/System/Library/Fonts/Menlo.ttc"],
        "monaco" => vec!["/System/Library/Fonts/Monaco.ttf"],
        "sfmono" | "sfnsmono" => vec!["/System/Library/Fonts/SFNSMono.ttf"],
        "couriernew" => vec!["/System/Library/Fonts/Supplemental/Courier New.ttf"],
        "courier" => vec!["/System/Library/Fonts/Courier.ttc"],
        "dejavusansmono" => vec![
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        ],
        "liberationmono" => {
            vec!["/usr/share/fonts/truetype/liberation2/LiberationMono-Regular.ttf"]
        }
        _ => Vec::new(),
    }
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
                .map(|entry| entry.to_ascii_lowercase())
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
    let path = recordings_root_dir()
        .join(id.to_string())
        .join("events.jsonl");
    let bytes = std::fs::read(&path)
        .with_context(|| format!("failed reading recording events file {}", path.display()))?;
    let mut events = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let event: RecordingEventEnvelope = serde_json::from_slice(line)
            .with_context(|| format!("failed parsing recording event in {}", path.display()))?;
        events.push(event);
    }
    Ok(events)
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
    for entry in std::fs::read_dir(&root)
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

fn recordings_root_dir() -> PathBuf {
    let paths = ConfigPaths::default();
    BmuxConfig::load_from_path(&paths.config_file())
        .map(|config| config.recordings_dir(&paths))
        .unwrap_or_else(|_| paths.recordings_dir())
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum DisplayTrackEvent {
    StreamOpened {
        client_id: Uuid,
        recording_id: Uuid,
        cell_width_px: Option<u16>,
        cell_height_px: Option<u16>,
        window_width_px: Option<u16>,
        window_height_px: Option<u16>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    FrameBytes {
        data: Vec<u8>,
    },
    StreamClosed,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DisplayTrackEnvelope {
    mono_ns: u64,
    event: DisplayTrackEvent,
}

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
        capture.record(DisplayTrackEvent::StreamOpened {
            client_id,
            recording_id: plan.recording_id,
            cell_width_px,
            cell_height_px,
            window_width_px,
            window_height_px,
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
        serde_json::to_writer(&mut self.writer, &envelope)?;
        self.writer.write_all(b"\n")?;
        Ok(())
    }
}

fn display_track_path(recording_path: &Path, client_id: Uuid) -> PathBuf {
    recording_path.join(format!("display-{client_id}.jsonl"))
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
            },
        }
    }

    #[test]
    fn parse_legacy_stream_opened_defaults_new_fields_to_none() {
        let parsed: DisplayTrackEnvelope = serde_json::from_str(
            r#"{"mono_ns":1,"event":{"kind":"stream_opened","client_id":"00000000-0000-0000-0000-000000000000","recording_id":"00000000-0000-0000-0000-000000000000"}}"#,
        )
        .expect("legacy stream_opened should deserialize");
        let DisplayTrackEvent::StreamOpened {
            cell_width_px,
            cell_height_px,
            window_width_px,
            window_height_px,
            ..
        } = parsed.event
        else {
            panic!("expected stream_opened event");
        };
        assert_eq!(cell_width_px, None);
        assert_eq!(cell_height_px, None);
        assert_eq!(window_width_px, None);
        assert_eq!(window_height_px, None);
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
}
