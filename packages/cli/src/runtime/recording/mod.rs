use super::*;

pub(super) async fn run_recording_start(
    session_id: Option<&str>,
    capture_input: bool,
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
    let summary = client
        .recording_start(session_id, capture_input)
        .await
        .map_err(map_cli_client_error)?;
    println!(
        "recording started: {} (capture_input={})",
        summary.id, summary.capture_input
    );
    Ok(0)
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
            "active recording: {} events={} bytes={} capture_input={} path={}",
            active.id, active.event_count, active.payload_bytes, active.capture_input, active.path
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
            "{} started={} ended={} events={} bytes={} capture_input={} path={}",
            recording.id,
            recording.started_epoch_ms,
            recording
                .ended_epoch_ms
                .map_or_else(|| "active".to_string(), |value| value.to_string()),
            recording.event_count,
            recording.payload_bytes,
            recording.capture_input,
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
) -> Result<u8> {
    let recording_id = parse_uuid_value(recording_id, "recording id")?;
    let recording_dir = ConfigPaths::default()
        .recordings_dir()
        .join(recording_id.to_string());
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
        RecordingExportFormat::Gif => {
            export_recording_gif(&events, output, speed, fps, max_duration, max_frames)?
        }
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

fn export_recording_gif(
    events: &[DisplayTrackEnvelope],
    output: &str,
    speed: f64,
    fps: u32,
    max_duration: Option<u64>,
    max_frames: Option<u32>,
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

    let cell_w = 8_u16;
    let cell_h = 8_u16;
    let width = max_cols.saturating_mul(cell_w).max(8);
    let height = max_rows.saturating_mul(cell_h).max(8);
    let palette = xterm_256_palette_rgb();

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
        GifEncoder::new(file, width, height, &palette).context("failed creating gif encoder")?;
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
                let pixels = render_screen_indexed(
                    parser.screen(),
                    current_rows,
                    current_cols,
                    max_rows,
                    max_cols,
                );
                let mut frame =
                    GifFrame::from_palette_pixels(width, height, pixels, palette.clone(), None);
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

fn render_screen_indexed(
    screen: &vt100::Screen,
    rows: u16,
    cols: u16,
    max_rows: u16,
    max_cols: u16,
) -> Vec<u8> {
    let width = usize::from(max_cols.saturating_mul(8));
    let height = usize::from(max_rows.saturating_mul(8));
    let mut pixels = vec![0_u8; width.saturating_mul(height)];

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
            let x0 = usize::from(col).saturating_mul(8);
            let y0 = usize::from(row).saturating_mul(8);
            for py in 0..8_usize {
                let y = y0 + py;
                if y >= height {
                    continue;
                }
                let row_start = y.saturating_mul(width);
                for px in 0..8_usize {
                    let x = x0 + px;
                    if x >= width {
                        continue;
                    }
                    pixels[row_start + x] = bg;
                }
            }

            let ch = if cell.has_contents() {
                cell.contents().chars().next().unwrap_or(' ')
            } else {
                ' '
            };
            let glyph = font8x8::BASIC_FONTS
                .get(ch)
                .or_else(|| font8x8::BASIC_FONTS.get('?'))
                .unwrap_or([0_u8; 8]);
            for (py, bits) in glyph.iter().copied().enumerate() {
                let y = y0 + py;
                if y >= height {
                    continue;
                }
                let row_start = y.saturating_mul(width);
                for px in 0..8_usize {
                    let x = x0 + px;
                    if x >= width {
                        continue;
                    }
                    if ((bits >> px) & 1) == 1 {
                        pixels[row_start + x] = fg;
                    }
                }
            }
        }
    }

    pixels
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

fn xterm_256_palette_rgb() -> Vec<u8> {
    let mut palette = Vec::with_capacity(256 * 3);
    for (r, g, b) in xterm_256_palette() {
        palette.push(r);
        palette.push(g);
        palette.push(b);
    }
    palette
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
    }
    .to_string()
}

pub(super) fn load_recording_events(recording_id: &str) -> Result<Vec<RecordingEventEnvelope>> {
    let id = Uuid::parse_str(recording_id).context("invalid recording id")?;
    let path = ConfigPaths::default()
        .recordings_dir()
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
    delete_recording_dir_at(&ConfigPaths::default().recordings_dir(), recording_id)
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
    delete_all_recordings_from_dir(&ConfigPaths::default().recordings_dir())
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
    list_recordings_from_dir(&ConfigPaths::default().recordings_dir())
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
    StreamOpened { client_id: Uuid, recording_id: Uuid },
    Resize { cols: u16, rows: u16 },
    FrameBytes { data: Vec<u8> },
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
        capture.record(DisplayTrackEvent::StreamOpened {
            client_id,
            recording_id: plan.recording_id,
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
