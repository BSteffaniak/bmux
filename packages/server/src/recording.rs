use anyhow::{Context, Result};
use bmux_ipc::{
    DisplayTrackEnvelope, DisplayTrackEvent, RecordingEventEnvelope, RecordingEventKind,
    RecordingPayload, RecordingProfile, RecordingStatus, RecordingSummary,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const MANIFEST_FILE_NAME: &str = "manifest.json";
const DEFAULT_ROLLING_SEGMENT_MAX_AGE_SECS: u64 = 2;

#[derive(Debug)]
pub struct RecordingRuntime {
    root_dir: PathBuf,
    active: Option<ActiveRecording>,
    segment_mb: usize,
    retention_days: u64,
    rolling_window_secs: Option<u64>,
    rolling_segment_max_age_secs: Option<u64>,
}

#[derive(Debug)]
struct ActiveRecording {
    id: Uuid,
    name: Option<String>,
    session_filter: Option<Uuid>,
    capture_input: bool,
    profile: RecordingProfile,
    event_kinds: Vec<RecordingEventKind>,
    started_epoch_ms: u64,
    started_at: Instant,
    seq: AtomicU64,
    event_count: Arc<AtomicU64>,
    payload_bytes: Arc<AtomicU64>,
    sender: mpsc::Sender<RecordingEventEnvelope>,
    writer: Option<thread::JoinHandle<Result<RecordingSummary>>>,
    path: PathBuf,
}

#[derive(Debug, Clone, Copy)]
pub struct RecordMeta {
    pub session_id: Option<Uuid>,
    pub pane_id: Option<Uuid>,
    pub client_id: Option<Uuid>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    summary: RecordingSummary,
}

impl RecordingRuntime {
    #[must_use]
    pub const fn new(root_dir: PathBuf, segment_mb: usize, retention_days: u64) -> Self {
        Self {
            root_dir,
            active: None,
            segment_mb,
            retention_days,
            rolling_window_secs: None,
            rolling_segment_max_age_secs: None,
        }
    }

    #[must_use]
    pub const fn new_rolling(
        root_dir: PathBuf,
        segment_mb: usize,
        rolling_window_secs: u64,
    ) -> Self {
        Self {
            root_dir,
            active: None,
            segment_mb,
            retention_days: 0,
            rolling_window_secs: Some(rolling_window_secs),
            rolling_segment_max_age_secs: Some(DEFAULT_ROLLING_SEGMENT_MAX_AGE_SECS),
        }
    }

    /// Start a new recording session.
    ///
    /// # Errors
    /// Returns an error if a recording is already active or if the recording
    /// directory cannot be created.
    pub fn start(
        &mut self,
        session_filter: Option<Uuid>,
        capture_input: bool,
        name: Option<String>,
        profile: RecordingProfile,
        event_kinds: Vec<RecordingEventKind>,
    ) -> Result<RecordingSummary> {
        if self.active.is_some() {
            anyhow::bail!("recording already active")
        }

        std::fs::create_dir_all(&self.root_dir).with_context(|| {
            format!(
                "failed creating recordings root {}",
                self.root_dir.display()
            )
        })?;

        let id = Uuid::new_v4();
        let name = normalize_recording_name(name)?;
        let started_epoch_ms = epoch_millis_now();
        let dir = self.root_dir.join(id.to_string());
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed creating recording dir {}", dir.display()))?;

        let manifest_path = dir.join(MANIFEST_FILE_NAME);
        let summary = RecordingSummary {
            id,
            name: name.clone(),
            format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
            session_id: session_filter,
            capture_input,
            profile,
            event_kinds: event_kinds.clone(),
            started_epoch_ms,
            ended_epoch_ms: None,
            event_count: 0,
            payload_bytes: 0,
            path: dir.to_string_lossy().to_string(),
            segments: vec![format!("events_0.bin")],
            total_segment_bytes: 0,
        };
        write_manifest(&manifest_path, &summary)?;

        let (tx, rx) = mpsc::channel::<RecordingEventEnvelope>();
        let event_count = Arc::new(AtomicU64::new(0));
        let payload_bytes = Arc::new(AtomicU64::new(0));
        let event_count_thread = Arc::clone(&event_count);
        let payload_bytes_thread = Arc::clone(&payload_bytes);
        let summary_for_thread = summary.clone();
        let segment_mb = self.segment_mb;
        let recording_dir = dir.clone();
        let rolling_window_secs = self.rolling_window_secs;
        let rolling_segment_max_age_secs = self.rolling_segment_max_age_secs;

        let writer = thread::Builder::new()
            .name(format!("bmux-recording-{id}"))
            .spawn(move || {
                writer_loop(
                    &rx,
                    &recording_dir,
                    &manifest_path,
                    summary_for_thread,
                    &event_count_thread,
                    &payload_bytes_thread,
                    segment_mb,
                    rolling_window_secs,
                    rolling_segment_max_age_secs,
                )
            })
            .context("failed to spawn recording writer thread")?;

        self.active = Some(ActiveRecording {
            id,
            name,
            session_filter,
            capture_input,
            profile,
            event_kinds,
            started_epoch_ms,
            started_at: Instant::now(),
            seq: AtomicU64::new(0),
            event_count,
            payload_bytes,
            sender: tx,
            writer: Some(writer),
            path: dir,
        });

        Ok(summary)
    }

    /// Stop the active recording.
    ///
    /// # Errors
    /// Returns an error if no recording is active or the writer thread panicked.
    pub fn stop(&mut self, recording_id: Option<Uuid>) -> Result<RecordingSummary> {
        let Some(mut active) = self.active.take() else {
            anyhow::bail!("no active recording")
        };

        if let Some(id) = recording_id
            && id != active.id
        {
            self.active = Some(active);
            anyhow::bail!("active recording id does not match requested id")
        }

        drop(active.sender);
        let Some(writer) = active.writer.take() else {
            anyhow::bail!("recording writer missing")
        };
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("recording writer thread panicked"))?
    }

    pub fn status(&self) -> RecordingStatus {
        let active = self.active.as_ref().map(|active| RecordingSummary {
            id: active.id,
            name: active.name.clone(),
            format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
            session_id: active.session_filter,
            capture_input: active.capture_input,
            profile: active.profile,
            event_kinds: active.event_kinds.clone(),
            started_epoch_ms: active.started_epoch_ms,
            ended_epoch_ms: None,
            event_count: active.event_count.load(Ordering::SeqCst),
            payload_bytes: active.payload_bytes.load(Ordering::SeqCst),
            path: active.path.to_string_lossy().to_string(),
            segments: vec!["events_0.bin".to_string()],
            total_segment_bytes: active.payload_bytes.load(Ordering::SeqCst),
        });
        RecordingStatus {
            active,
            queue_len: 0,
        }
    }

    /// List all known recordings.
    ///
    /// # Errors
    /// Returns an error if the recording directory cannot be read.
    pub fn list(&self) -> Result<Vec<RecordingSummary>> {
        let mut recordings = Vec::new();
        if self.root_dir.exists() {
            for entry in std::fs::read_dir(&self.root_dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let manifest_path = entry.path().join(MANIFEST_FILE_NAME);
                if !manifest_path.exists() {
                    continue;
                }
                if let Ok(summary) = read_manifest(&manifest_path) {
                    recordings.push(summary);
                }
            }
        }
        if let Some(active) = self.status().active {
            if let Some(index) = recordings.iter().position(|r| r.id == active.id) {
                recordings[index] = active;
            } else {
                recordings.push(active);
            }
        }
        recordings.sort_by(|a, b| b.started_epoch_ms.cmp(&a.started_epoch_ms));
        Ok(recordings)
    }

    /// Delete a specific recording by ID.
    ///
    /// # Errors
    /// Returns an error if the recording is not found or cannot be removed.
    pub fn delete(&mut self, recording_id: Uuid) -> Result<RecordingSummary> {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.id == recording_id)
        {
            let _ = self.stop(Some(recording_id))?;
        }

        let recording_dir = self.root_dir.join(recording_id.to_string());
        let manifest_path = recording_dir.join(MANIFEST_FILE_NAME);
        if !manifest_path.exists() {
            anyhow::bail!("recording not found: {recording_id}")
        }
        let summary = read_manifest(&manifest_path)?;

        std::fs::remove_dir_all(&recording_dir).with_context(|| {
            format!(
                "failed removing recording directory {}",
                recording_dir.display()
            )
        })?;

        Ok(summary)
    }

    /// Delete all recordings.
    ///
    /// # Errors
    /// Returns an error if the recording directory cannot be read or entries removed.
    pub fn delete_all(&mut self) -> Result<usize> {
        if self.active.is_some() {
            let _ = self.stop(None)?;
        }

        if !self.root_dir.exists() {
            return Ok(0);
        }

        let mut deleted = 0_usize;
        for entry in std::fs::read_dir(&self.root_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let manifest_path = entry.path().join(MANIFEST_FILE_NAME);
            if !manifest_path.exists() {
                continue;
            }
            std::fs::remove_dir_all(entry.path()).with_context(|| {
                format!(
                    "failed removing recording directory {}",
                    entry.path().display()
                )
            })?;
            deleted = deleted.saturating_add(1);
        }

        Ok(deleted)
    }

    /// Record a single event into the active recording.
    ///
    /// # Errors
    /// Returns an error if the recording writer is not accepting events.
    pub fn record(
        &self,
        kind: RecordingEventKind,
        payload: RecordingPayload,
        meta: RecordMeta,
    ) -> Result<bool> {
        let Some(active) = self.active.as_ref() else {
            return Ok(false);
        };

        if let Some(filter) = active.session_filter
            && meta.session_id != Some(filter)
        {
            return Ok(false);
        }

        if !active.event_kinds.contains(&kind) {
            return Ok(false);
        }

        if matches!(kind, RecordingEventKind::PaneInputRaw) && !active.capture_input {
            return Ok(false);
        }

        // Sample the clock BEFORE incrementing seq to ensure that if thread A
        // gets seq=N and thread B gets seq=N+1, A's mono_ns <= B's mono_ns
        // (because A read the clock first in wall-clock time, before B could
        // observe the incremented counter).
        let elapsed = active.started_at.elapsed();
        let seq = active.seq.fetch_add(1, Ordering::SeqCst).saturating_add(1);
        let envelope = RecordingEventEnvelope {
            seq,
            #[allow(clippy::cast_possible_truncation)]
            mono_ns: elapsed.as_nanos().min(u128::from(u64::MAX)) as u64,
            wall_epoch_ms: epoch_millis_now(),
            session_id: meta.session_id,
            pane_id: meta.pane_id,
            client_id: meta.client_id,
            kind,
            payload,
        };
        active
            .sender
            .send(envelope)
            .map_err(|_| anyhow::anyhow!("recording writer is not accepting events"))?;
        Ok(true)
    }

    /// Prune completed recordings older than the specified retention period.
    /// Returns the number of recordings deleted.
    ///
    /// # Errors
    /// Returns an error if the recordings directory cannot be read.
    pub fn prune(&self, older_than_days: Option<u64>) -> Result<usize> {
        let retention = older_than_days.unwrap_or(self.retention_days);
        prune_old_recordings(&self.root_dir, retention)
    }

    /// Get the root recordings directory.
    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    pub fn active_capture_target(&self) -> Option<(Uuid, PathBuf)> {
        self.active
            .as_ref()
            .map(|active| (active.id, active.path.clone()))
    }

    pub const fn rolling_window_secs(&self) -> Option<u64> {
        self.rolling_window_secs
    }

    /// Get the configured retention days.
    pub const fn retention_days(&self) -> u64 {
        self.retention_days
    }

    /// Cut a snapshot of the rolling recording for the given time window.
    ///
    /// # Errors
    /// Returns an error if no recording is active, the window is invalid,
    /// or file I/O fails.
    pub fn cut(
        &self,
        output_root: &Path,
        last_seconds: Option<u64>,
        name: Option<String>,
    ) -> Result<RecordingSummary> {
        let active = self
            .active
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no active recording available for cut"))?;
        let name = normalize_recording_name(name)?.or_else(|| active.name.clone());
        let window_secs = last_seconds.or(self.rolling_window_secs).ok_or_else(|| {
            anyhow::anyhow!("recording cut requires rolling recording mode to be enabled")
        })?;
        if window_secs == 0 {
            anyhow::bail!("recording cut window must be greater than zero seconds")
        }

        let cutoff_ms = epoch_millis_now().saturating_sub(window_secs.saturating_mul(1000));
        let segment_names = list_segment_names(&active.path)?;
        let mut events = Vec::new();
        for name in &segment_names {
            let path = active.path.join(name);
            let mut segment_events = read_recording_events(&path)?;
            events.append(&mut segment_events);
        }

        events.retain(|event| event.wall_epoch_ms >= cutoff_ms);
        if events.is_empty() {
            anyhow::bail!("rolling recording has no events in the requested window")
        }

        let first_mono_ns = events.first().map_or(0, |event| event.mono_ns);
        for (index, event) in events.iter_mut().enumerate() {
            event.seq = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            event.mono_ns = event.mono_ns.saturating_sub(first_mono_ns);
        }

        std::fs::create_dir_all(output_root).with_context(|| {
            format!(
                "failed creating recording cut output root {}",
                output_root.display()
            )
        })?;

        let id = Uuid::new_v4();
        let cut_dir = output_root.join(id.to_string());
        std::fs::create_dir_all(&cut_dir).with_context(|| {
            format!(
                "failed creating recording cut directory {}",
                cut_dir.display()
            )
        })?;

        let segments = write_recording_events_with_rotation(&cut_dir, &events, self.segment_mb)?;
        let event_count = u64::try_from(events.len()).unwrap_or(u64::MAX);
        let payload_bytes = events
            .iter()
            .map(|event| payload_size(&event.payload))
            .sum::<u64>();
        let summary = RecordingSummary {
            id,
            name,
            format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
            session_id: active.session_filter,
            capture_input: active.capture_input,
            profile: active.profile,
            event_kinds: active.event_kinds.clone(),
            started_epoch_ms: events
                .first()
                .map_or_else(epoch_millis_now, |event| event.wall_epoch_ms),
            ended_epoch_ms: Some(epoch_millis_now()),
            event_count,
            payload_bytes,
            path: cut_dir.to_string_lossy().to_string(),
            segments,
            total_segment_bytes: 0,
        };

        copy_display_tracks_for_cut(&active.path, &cut_dir, window_secs)?;
        copy_owner_client_metadata(&active.path, &cut_dir)?;

        let mut finalized = summary;
        finalized.total_segment_bytes = compute_total_segment_bytes(&cut_dir, &finalized.segments);
        write_manifest(&cut_dir.join(MANIFEST_FILE_NAME), &finalized)?;
        Ok(finalized)
    }
}

fn normalize_recording_name(name: Option<String>) -> Result<Option<String>> {
    let Some(name) = name else {
        return Ok(None);
    };
    let trimmed = name.trim();
    if trimmed.is_empty() {
        anyhow::bail!("recording name cannot be empty")
    }
    Ok(Some(trimmed.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn writer_loop(
    rx: &mpsc::Receiver<RecordingEventEnvelope>,
    recording_dir: &Path,
    manifest_path: &Path,
    mut summary: RecordingSummary,
    event_count: &Arc<AtomicU64>,
    payload_bytes: &Arc<AtomicU64>,
    segment_mb: usize,
    rolling_window_secs: Option<u64>,
    rolling_segment_max_age_secs: Option<u64>,
) -> Result<RecordingSummary> {
    let segment_limit_bytes = (segment_mb as u64) * 1024 * 1024;
    let rolling_window_ms = rolling_window_secs.map(|secs| secs.saturating_mul(1000));
    let rolling_segment_max_age_ms =
        rolling_segment_max_age_secs.map(|secs| secs.saturating_mul(1000));
    let mut segment_index: usize = 0;
    let mut segment_bytes: u64 = 0;
    let mut current_segment_start_wall_ms: Option<u64> = None;
    let mut closed_segments: VecDeque<(String, u64)> = VecDeque::new();

    let mut segment_name = format!("events_{segment_index}.bin");
    let segment_path = recording_dir.join(&segment_name);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&segment_path)
        .with_context(|| format!("failed opening segment file {}", segment_path.display()))?;
    let mut writer = BufWriter::new(file);
    summary.segments = vec![segment_name.clone()];

    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(event) => {
                bmux_ipc::write_frame(&mut writer, &event)
                    .map_err(|e| anyhow::anyhow!("recording write_frame failed: {e}"))?;
                let payload_sz = payload_size(&event.payload);
                event_count.fetch_add(1, Ordering::SeqCst);
                payload_bytes.fetch_add(payload_sz, Ordering::SeqCst);
                segment_bytes = segment_bytes.saturating_add(payload_sz);

                if current_segment_start_wall_ms.is_none() {
                    current_segment_start_wall_ms = Some(event.wall_epoch_ms);
                }

                // Periodic flush + manifest update.
                if event.seq % 128 == 0 {
                    writer.flush()?;
                    summary.event_count = event_count.load(Ordering::SeqCst);
                    summary.payload_bytes = payload_bytes.load(Ordering::SeqCst);
                    write_manifest(manifest_path, &summary)?;
                }

                let rotate_by_size =
                    segment_limit_bytes > 0 && segment_bytes >= segment_limit_bytes;
                let rotate_by_age = rolling_segment_max_age_ms.is_some_and(|max_age_ms| {
                    current_segment_start_wall_ms.is_some_and(|start_ms| {
                        event.wall_epoch_ms.saturating_sub(start_ms) >= max_age_ms
                    })
                });

                if rotate_by_size || rotate_by_age {
                    writer.flush()?;
                    drop(writer);

                    closed_segments.push_back((segment_name.clone(), event.wall_epoch_ms));

                    segment_index = segment_index.saturating_add(1);
                    segment_bytes = 0;
                    current_segment_start_wall_ms = None;
                    segment_name = format!("events_{segment_index}.bin");
                    let new_segment_path = recording_dir.join(&segment_name);
                    let new_file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&new_segment_path)
                        .with_context(|| {
                            format!(
                                "failed opening new segment file {}",
                                new_segment_path.display()
                            )
                        })?;
                    writer = BufWriter::new(new_file);
                    summary.segments.push(segment_name.clone());

                    if let Some(window_ms) = rolling_window_ms {
                        prune_closed_segments(
                            recording_dir,
                            &mut summary,
                            &mut closed_segments,
                            epoch_millis_now(),
                            window_ms,
                        )?;
                    }

                    // Update manifest with new segment list.
                    summary.event_count = event_count.load(Ordering::SeqCst);
                    summary.payload_bytes = payload_bytes.load(Ordering::SeqCst);
                    write_manifest(manifest_path, &summary)?;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(window_ms) = rolling_window_ms {
                    writer.flush()?;
                    prune_closed_segments(
                        recording_dir,
                        &mut summary,
                        &mut closed_segments,
                        epoch_millis_now(),
                        window_ms,
                    )?;
                    write_manifest(manifest_path, &summary)?;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Final flush and manifest update.
    writer.flush()?;
    summary.event_count = event_count.load(Ordering::SeqCst);
    summary.payload_bytes = payload_bytes.load(Ordering::SeqCst);
    summary.total_segment_bytes = compute_total_segment_bytes(recording_dir, &summary.segments);
    summary.ended_epoch_ms = Some(epoch_millis_now());
    write_manifest(manifest_path, &summary)?;
    Ok(summary)
}

fn prune_closed_segments(
    recording_dir: &Path,
    summary: &mut RecordingSummary,
    closed_segments: &mut VecDeque<(String, u64)>,
    now_ms: u64,
    window_ms: u64,
) -> Result<()> {
    let cutoff_ms = now_ms.saturating_sub(window_ms);
    let mut removed_any = false;

    while let Some((name, end_ms)) = closed_segments.front().cloned() {
        if end_ms >= cutoff_ms {
            break;
        }
        let _ = closed_segments.pop_front();

        let segment_path = recording_dir.join(&name);
        if let Err(error) = std::fs::remove_file(&segment_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error).with_context(|| {
                format!("failed removing old segment {}", segment_path.display())
            });
        }

        if let Some(index) = summary.segments.iter().position(|segment| segment == &name) {
            summary.segments.remove(index);
        }
        removed_any = true;
    }

    if removed_any {
        summary.total_segment_bytes = compute_total_segment_bytes(recording_dir, &summary.segments);
    }

    Ok(())
}

/// Compute the total size of all segment files on disk.
fn compute_total_segment_bytes(recording_dir: &Path, segments: &[String]) -> u64 {
    segments
        .iter()
        .map(|name| {
            std::fs::metadata(recording_dir.join(name))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .sum()
}

fn list_segment_names(recording_dir: &Path) -> Result<Vec<String>> {
    let mut indexed = Vec::new();
    for entry in std::fs::read_dir(recording_dir).with_context(|| {
        format!(
            "failed reading recording directory {}",
            recording_dir.display()
        )
    })? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("events_") else {
            continue;
        };
        let Some(index_raw) = rest.strip_suffix(".bin") else {
            continue;
        };
        let Ok(index) = index_raw.parse::<u64>() else {
            continue;
        };
        indexed.push((index, name));
    }
    indexed.sort_by_key(|(index, _)| *index);
    Ok(indexed.into_iter().map(|(_, name)| name).collect())
}

fn read_recording_events(path: &Path) -> Result<Vec<RecordingEventEnvelope>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed reading recording segment {}", path.display()))?;
    let result = bmux_ipc::read_frames::<RecordingEventEnvelope>(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "failed parsing recording segment {}: {error}",
            path.display()
        )
    })?;
    Ok(result.frames)
}

fn write_recording_events_with_rotation(
    recording_dir: &Path,
    events: &[RecordingEventEnvelope],
    segment_mb: usize,
) -> Result<Vec<String>> {
    let segment_limit_bytes = (segment_mb as u64) * 1024 * 1024;
    let mut segment_index = 0usize;
    let mut segment_bytes = 0u64;
    let mut segment_names = Vec::new();

    let mut segment_name = format!("events_{segment_index}.bin");
    let mut writer = BufWriter::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(recording_dir.join(&segment_name))
            .with_context(|| {
                format!(
                    "failed opening recording cut segment {}",
                    recording_dir.join(&segment_name).display()
                )
            })?,
    );
    segment_names.push(segment_name.clone());

    for event in events {
        bmux_ipc::write_frame(&mut writer, event)
            .map_err(|error| anyhow::anyhow!("recording cut write_frame failed: {error}"))?;
        segment_bytes = segment_bytes.saturating_add(payload_size(&event.payload));
        if segment_limit_bytes > 0 && segment_bytes >= segment_limit_bytes {
            writer.flush()?;
            segment_index = segment_index.saturating_add(1);
            segment_name = format!("events_{segment_index}.bin");
            writer = BufWriter::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(recording_dir.join(&segment_name))
                    .with_context(|| {
                        format!(
                            "failed opening recording cut segment {}",
                            recording_dir.join(&segment_name).display()
                        )
                    })?,
            );
            segment_names.push(segment_name.clone());
            segment_bytes = 0;
        }
    }

    writer.flush()?;
    Ok(segment_names)
}

fn copy_owner_client_metadata(source_dir: &Path, dest_dir: &Path) -> Result<()> {
    let owner_path = source_dir.join("owner-client-id.txt");
    if !owner_path.exists() {
        return Ok(());
    }
    std::fs::copy(&owner_path, dest_dir.join("owner-client-id.txt")).with_context(|| {
        format!(
            "failed copying owner client metadata from {}",
            owner_path.display()
        )
    })?;
    Ok(())
}

fn push_cut_stream_opened_baseline(
    output: &mut Vec<DisplayTrackEnvelope>,
    all_frames: &[DisplayTrackEnvelope],
    kept_first: Option<&DisplayTrackEnvelope>,
) {
    if kept_first.is_some_and(|frame| matches!(frame.event, DisplayTrackEvent::StreamOpened { .. }))
    {
        return;
    }
    if let Some(opened) = all_frames
        .iter()
        .find(|frame| matches!(frame.event, DisplayTrackEvent::StreamOpened { .. }))
        .cloned()
    {
        output.push(DisplayTrackEnvelope {
            mono_ns: 0,
            event: opened.event,
        });
    }
}

fn push_cut_resize_baseline_if_missing(
    output: &mut Vec<DisplayTrackEnvelope>,
    all_frames: &[DisplayTrackEnvelope],
    first_kept_ns: u64,
    kept_has_resize: bool,
) {
    if kept_has_resize {
        return;
    }
    if let Some(resize) = all_frames
        .iter()
        .rev()
        .find(|frame| {
            frame.mono_ns <= first_kept_ns
                && matches!(frame.event, DisplayTrackEvent::Resize { .. })
        })
        .cloned()
    {
        output.push(DisplayTrackEnvelope {
            mono_ns: 0,
            event: resize.event,
        });
    }
}

fn cut_display_track_frames(
    all_frames: &[DisplayTrackEnvelope],
    window_ns: u64,
) -> Vec<DisplayTrackEnvelope> {
    if all_frames.is_empty() {
        return Vec::new();
    }

    let last_ns = all_frames.last().map_or(0, |frame| frame.mono_ns);
    let cutoff_ns = last_ns.saturating_sub(window_ns);
    let mut kept: Vec<DisplayTrackEnvelope> = all_frames
        .iter()
        .filter(|frame| frame.mono_ns >= cutoff_ns)
        .cloned()
        .collect();
    if kept.is_empty() {
        return Vec::new();
    }

    let first_kept_ns = kept.first().map_or(0, |frame| frame.mono_ns);
    let kept_has_resize = kept
        .iter()
        .any(|frame| matches!(frame.event, DisplayTrackEvent::Resize { .. }));
    let mut output = Vec::new();
    push_cut_stream_opened_baseline(&mut output, all_frames, kept.first());
    push_cut_resize_baseline_if_missing(&mut output, all_frames, first_kept_ns, kept_has_resize);

    for frame in &mut kept {
        frame.mono_ns = frame.mono_ns.saturating_sub(first_kept_ns);
    }
    output.extend(kept);
    output
}

fn copy_display_tracks_for_cut(source_dir: &Path, dest_dir: &Path, window_secs: u64) -> Result<()> {
    if window_secs == 0 {
        return Ok(());
    }
    let window_ns = window_secs.saturating_mul(1_000_000_000);
    for entry in std::fs::read_dir(source_dir).with_context(|| {
        format!(
            "failed reading recording directory {}",
            source_dir.display()
        )
    })? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("display-")
            || !std::path::Path::new(&name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("bin"))
        {
            continue;
        }

        let path = entry.path();
        let bytes = std::fs::read(&path)
            .with_context(|| format!("failed reading display track {}", path.display()))?;
        let result = bmux_ipc::read_frames::<DisplayTrackEnvelope>(&bytes).map_err(|error| {
            anyhow::anyhow!("failed parsing display track {}: {error}", path.display())
        })?;
        let output = cut_display_track_frames(&result.frames, window_ns);

        if output.is_empty() {
            continue;
        }

        let mut writer = BufWriter::new(
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(dest_dir.join(&name))
                .with_context(|| {
                    format!(
                        "failed opening cut display track {}",
                        dest_dir.join(&name).display()
                    )
                })?,
        );
        for frame in &output {
            bmux_ipc::write_frame(&mut writer, frame).map_err(|error| {
                anyhow::anyhow!("failed writing cut display track frame: {error}")
            })?;
        }
        writer.flush()?;
    }
    Ok(())
}

fn write_manifest(path: &Path, summary: &RecordingSummary) -> Result<()> {
    let payload = Manifest {
        summary: summary.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&payload)?;
    std::fs::write(path, bytes)
        .with_context(|| format!("failed writing recording manifest {}", path.display()))
}

fn read_manifest(path: &Path) -> Result<RecordingSummary> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed reading recording manifest {}", path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    Ok(manifest.summary)
}

/// Approximate content size of a recording payload, used for manifest statistics.
///
/// This measures the size of the primary data fields in each payload variant,
/// not the actual serialized frame size (which includes the 4-byte length prefix
/// and codec envelope overhead). Treat `payload_bytes` in the manifest as
/// an approximate content-size metric, not an exact file-size measurement.
#[allow(clippy::missing_const_for_fn, clippy::cast_possible_truncation)]
fn payload_size(payload: &RecordingPayload) -> u64 {
    match payload {
        RecordingPayload::Bytes { data } | RecordingPayload::Image { data, .. } => {
            data.len() as u64
        }
        RecordingPayload::ServerEvent { .. } => {
            // Estimate: server events are typically small (< 256 bytes).
            // Avoid re-serializing just to measure size.
            128
        }
        RecordingPayload::RequestStart { request_data, .. } => request_data.len() as u64,
        RecordingPayload::RequestDone {
            request_data,
            response_data,
            ..
        } => (request_data.len() + response_data.len()) as u64,
        RecordingPayload::RequestError {
            request_kind,
            message,
            ..
        } => (request_kind.len() + message.len()) as u64,
        RecordingPayload::Custom {
            source,
            name,
            payload,
        } => (source.len() + name.len() + payload.len()) as u64,
    }
}

#[allow(clippy::cast_possible_truncation)]
fn epoch_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

/// Prune completed recordings older than `retention_days`.
/// Returns the number of recordings deleted. If `retention_days` is 0, returns 0
/// (0 means keep forever).
///
/// # Errors
/// Returns an error if the recordings directory cannot be read.
pub fn prune_old_recordings(root_dir: &Path, retention_days: u64) -> Result<usize> {
    if retention_days == 0 {
        return Ok(0);
    }
    if !root_dir.exists() {
        return Ok(0);
    }

    let cutoff_ms = epoch_millis_now().saturating_sub(retention_days * 24 * 60 * 60 * 1000);
    let mut deleted = 0;

    let entries = std::fs::read_dir(root_dir)
        .with_context(|| format!("failed reading recordings dir {}", root_dir.display()))?;

    for entry in entries {
        let Ok(entry) = entry else { continue };
        if !entry.path().is_dir() {
            continue;
        }
        let manifest_path = entry.path().join(MANIFEST_FILE_NAME);
        if !manifest_path.exists() {
            continue;
        }
        let Ok(summary) = read_manifest(&manifest_path) else {
            continue;
        };
        // Only prune completed recordings (has ended_epoch_ms).
        if let Some(ended_ms) = summary.ended_epoch_ms
            && ended_ms < cutoff_ms
        {
            if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                tracing::warn!("failed to prune recording {}: {e}", entry.path().display());
            } else {
                deleted += 1;
            }
        }
    }

    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::{Manifest, RecordMeta, RecordingRuntime};
    use bmux_ipc::{
        DisplayCursorShape, DisplayTrackEnvelope, DisplayTrackEvent, RecordingEventKind,
        RecordingPayload, RecordingProfile, RecordingSummary,
    };
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be monotonic for test")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bmux-recording-test-{nanos}"));
        fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    fn write_display_track(path: &Path, frames: &[DisplayTrackEnvelope]) {
        let mut bytes = Vec::new();
        for frame in frames {
            bmux_ipc::write_frame(&mut bytes, frame).expect("display frame should encode");
        }
        fs::write(path, bytes).expect("display track should write");
    }

    fn read_display_track(path: &Path) -> Vec<DisplayTrackEnvelope> {
        let bytes = fs::read(path).expect("display track should read");
        bmux_ipc::read_frames::<DisplayTrackEnvelope>(&bytes)
            .expect("display track should decode")
            .frames
    }

    fn stream_opened_frame(mono_ns: u64) -> DisplayTrackEnvelope {
        DisplayTrackEnvelope {
            mono_ns,
            event: DisplayTrackEvent::StreamOpened {
                client_id: Uuid::nil(),
                recording_id: Uuid::nil(),
                cell_width_px: Some(16),
                cell_height_px: Some(35),
                window_width_px: Some(3440),
                window_height_px: Some(2150),
                terminal_profile: None,
            },
        }
    }

    fn resize_frame(mono_ns: u64, cols: u16, rows: u16) -> DisplayTrackEnvelope {
        DisplayTrackEnvelope {
            mono_ns,
            event: DisplayTrackEvent::Resize { cols, rows },
        }
    }

    fn cursor_snapshot_frame(mono_ns: u64, x: u16, y: u16) -> DisplayTrackEnvelope {
        DisplayTrackEnvelope {
            mono_ns,
            event: DisplayTrackEvent::CursorSnapshot {
                x,
                y,
                visible: true,
                shape: DisplayCursorShape::Block,
                blink_enabled: true,
            },
        }
    }

    #[test]
    fn start_record_stop_persists_manifest() {
        let root = temp_dir();
        let mut runtime = RecordingRuntime::new(root.clone(), 64, 30);
        let summary = runtime
            .start(
                None,
                true,
                None,
                RecordingProfile::Functional,
                vec![RecordingEventKind::PaneOutputRaw],
            )
            .expect("recording should start");
        runtime
            .record(
                RecordingEventKind::PaneOutputRaw,
                RecordingPayload::Bytes {
                    data: b"hello".to_vec(),
                },
                RecordMeta {
                    session_id: Some(Uuid::new_v4()),
                    pane_id: Some(Uuid::new_v4()),
                    client_id: None,
                },
            )
            .expect("event should record");
        let stopped = runtime.stop(None).expect("recording should stop");
        assert_eq!(stopped.id, summary.id);
        assert!(stopped.ended_epoch_ms.is_some());
        assert!(stopped.event_count >= 1);
        assert!(
            root.join(stopped.id.to_string())
                .join("manifest.json")
                .exists(),
            "manifest should exist"
        );
    }

    #[test]
    fn start_with_name_persists_name() {
        let root = temp_dir();
        let mut runtime = RecordingRuntime::new(root.clone(), 64, 30);
        let summary = runtime
            .start(
                None,
                true,
                Some("startup-regression".to_string()),
                RecordingProfile::Functional,
                vec![RecordingEventKind::PaneOutputRaw],
            )
            .expect("recording should start");
        let stopped = runtime
            .stop(Some(summary.id))
            .expect("recording should stop");
        assert_eq!(stopped.name.as_deref(), Some("startup-regression"));
        let manifest_summary = super::read_manifest(
            &root
                .join(summary.id.to_string())
                .join(super::MANIFEST_FILE_NAME),
        )
        .expect("manifest should parse");
        assert_eq!(manifest_summary.name.as_deref(), Some("startup-regression"));
    }

    #[test]
    fn no_capture_input_suppresses_input_events() {
        let root = temp_dir();
        let mut runtime = RecordingRuntime::new(root, 64, 30);
        runtime
            .start(
                None,
                false,
                None,
                RecordingProfile::Functional,
                vec![RecordingEventKind::PaneInputRaw],
            )
            .expect("recording should start");
        runtime
            .record(
                RecordingEventKind::PaneInputRaw,
                RecordingPayload::Bytes {
                    data: b"secret".to_vec(),
                },
                RecordMeta {
                    session_id: Some(Uuid::new_v4()),
                    pane_id: Some(Uuid::new_v4()),
                    client_id: Some(Uuid::new_v4()),
                },
            )
            .expect("record should no-op without failure");
        let stopped = runtime.stop(None).expect("recording should stop");
        assert_eq!(stopped.event_count, 0);
    }

    #[test]
    fn delete_removes_manifest_directory() {
        let root = temp_dir();
        let mut runtime = RecordingRuntime::new(root.clone(), 64, 30);
        let summary = runtime
            .start(
                None,
                true,
                None,
                RecordingProfile::Functional,
                vec![RecordingEventKind::PaneOutputRaw],
            )
            .expect("recording should start");
        runtime
            .stop(Some(summary.id))
            .expect("recording should stop");

        let deleted = runtime.delete(summary.id).expect("recording should delete");
        assert_eq!(deleted.id, summary.id);
        assert!(!root.join(summary.id.to_string()).exists());
    }

    #[test]
    fn delete_all_stops_active_and_removes_recordings() {
        let root = temp_dir();
        let mut runtime = RecordingRuntime::new(root, 64, 30);
        let active = runtime
            .start(
                None,
                true,
                None,
                RecordingProfile::Functional,
                vec![RecordingEventKind::PaneOutputRaw],
            )
            .expect("first recording should start");
        let second = runtime
            .start(
                None,
                true,
                None,
                RecordingProfile::Functional,
                vec![RecordingEventKind::PaneOutputRaw],
            )
            .err();
        assert!(second.is_some(), "second concurrent recording should fail");

        let _ = runtime
            .stop(Some(active.id))
            .expect("active recording should stop");
        let _ = runtime
            .start(
                None,
                true,
                None,
                RecordingProfile::Functional,
                vec![RecordingEventKind::PaneOutputRaw],
            )
            .expect("new active should start");

        let deleted_count = runtime.delete_all().expect("delete all should succeed");
        assert_eq!(deleted_count, 2);
        assert!(runtime.status().active.is_none());
        assert!(runtime.list().expect("list should succeed").is_empty());
    }

    #[test]
    fn segment_rotation_creates_files() {
        let root = temp_dir();
        let mut runtime = RecordingRuntime::new(root.clone(), 64, 30);

        let summary = runtime
            .start(
                None,
                true,
                None,
                RecordingProfile::Full,
                vec![RecordingEventKind::PaneOutputRaw],
            )
            .expect("start recording");
        assert_eq!(summary.segments, vec!["events_0.bin"]);

        // Write some events.
        runtime
            .record(
                RecordingEventKind::PaneOutputRaw,
                RecordingPayload::Bytes {
                    data: b"hello".to_vec(),
                },
                RecordMeta {
                    session_id: None,
                    pane_id: None,
                    client_id: None,
                },
            )
            .unwrap();

        let stopped = runtime.stop(None).expect("stop recording");
        assert!(stopped.event_count >= 1);
        assert!(!stopped.segments.is_empty());
        // Verify the first segment file exists on disk.
        let seg_path = root.join(stopped.id.to_string()).join(&stopped.segments[0]);
        assert!(
            seg_path.exists(),
            "segment file should exist: {}",
            seg_path.display()
        );
    }

    #[test]
    fn prune_old_recordings_deletes_expired() {
        let root = temp_dir();

        let rec_id = Uuid::new_v4();
        let rec_dir = root.join(rec_id.to_string());
        std::fs::create_dir_all(&rec_dir).unwrap();
        let summary = RecordingSummary {
            id: rec_id,
            name: None,
            format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
            session_id: None,
            capture_input: true,
            profile: RecordingProfile::Full,
            event_kinds: vec![],
            started_epoch_ms: 1,
            ended_epoch_ms: Some(1_000_000),
            event_count: 0,
            payload_bytes: 0,
            path: rec_dir.to_string_lossy().to_string(),
            segments: vec!["events_0.bin".to_string()],
            total_segment_bytes: 0,
        };
        let manifest = Manifest { summary };
        std::fs::write(
            rec_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let deleted = super::prune_old_recordings(&root, 1).unwrap();
        assert_eq!(deleted, 1);
        assert!(!rec_dir.exists());
    }

    #[test]
    fn prune_skips_active_recordings() {
        let root = temp_dir();

        let rec_id = Uuid::new_v4();
        let rec_dir = root.join(rec_id.to_string());
        std::fs::create_dir_all(&rec_dir).unwrap();
        let summary = RecordingSummary {
            id: rec_id,
            name: None,
            format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
            session_id: None,
            capture_input: true,
            profile: RecordingProfile::Full,
            event_kinds: vec![],
            started_epoch_ms: 1,
            ended_epoch_ms: None,
            event_count: 0,
            payload_bytes: 0,
            path: rec_dir.to_string_lossy().to_string(),
            segments: vec!["events_0.bin".to_string()],
            total_segment_bytes: 0,
        };
        let manifest = Manifest { summary };
        std::fs::write(
            rec_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let deleted = super::prune_old_recordings(&root, 1).unwrap();
        assert_eq!(deleted, 0);
        assert!(rec_dir.exists());
    }

    #[test]
    fn prune_zero_retention_keeps_all() {
        let root = temp_dir();

        let rec_id = Uuid::new_v4();
        let rec_dir = root.join(rec_id.to_string());
        std::fs::create_dir_all(&rec_dir).unwrap();
        let summary = RecordingSummary {
            id: rec_id,
            name: None,
            format_version: bmux_ipc::RECORDING_FORMAT_VERSION,
            session_id: None,
            capture_input: true,
            profile: RecordingProfile::Full,
            event_kinds: vec![],
            started_epoch_ms: 1,
            ended_epoch_ms: Some(1_000_000),
            event_count: 0,
            payload_bytes: 0,
            path: rec_dir.to_string_lossy().to_string(),
            segments: vec!["events_0.bin".to_string()],
            total_segment_bytes: 0,
        };
        let manifest = Manifest { summary };
        std::fs::write(
            rec_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let deleted = super::prune_old_recordings(&root, 0).unwrap();
        assert_eq!(deleted, 0);
        assert!(rec_dir.exists());
    }

    #[test]
    fn copy_display_tracks_for_cut_injects_baseline_resize_when_window_has_none() {
        let root = temp_dir();
        let source_dir = root.join("source");
        let cut_dir = root.join("cut");
        fs::create_dir_all(&source_dir).expect("source dir should exist");
        fs::create_dir_all(&cut_dir).expect("cut dir should exist");

        let client_id = Uuid::new_v4();
        let display_name = format!("display-{client_id}.bin");
        write_display_track(
            &source_dir.join(&display_name),
            &[
                stream_opened_frame(0),
                resize_frame(1_000, 215, 61),
                DisplayTrackEnvelope {
                    mono_ns: 30_000_000_000,
                    event: DisplayTrackEvent::FrameBytes { data: vec![b'x'] },
                },
                cursor_snapshot_frame(39_000_000_000, 213, 57),
                DisplayTrackEnvelope {
                    mono_ns: 40_000_000_000,
                    event: DisplayTrackEvent::StreamClosed,
                },
            ],
        );

        super::copy_display_tracks_for_cut(&source_dir, &cut_dir, 10)
            .expect("display track copy should succeed");

        let cut_frames = read_display_track(&cut_dir.join(display_name));
        assert!(
            matches!(cut_frames.first(), Some(frame) if matches!(frame.event, DisplayTrackEvent::StreamOpened { .. })),
            "cut track should start with stream_opened"
        );
        assert!(
            matches!(cut_frames.get(1), Some(frame) if matches!(frame.event, DisplayTrackEvent::Resize { cols: 215, rows: 61 })),
            "cut track should include injected baseline resize"
        );
        let resize_count = cut_frames
            .iter()
            .filter(|frame| matches!(frame.event, DisplayTrackEvent::Resize { .. }))
            .count();
        assert_eq!(resize_count, 1);
    }

    #[test]
    fn copy_display_tracks_for_cut_does_not_duplicate_resize_when_window_has_one() {
        let root = temp_dir();
        let source_dir = root.join("source");
        let cut_dir = root.join("cut");
        fs::create_dir_all(&source_dir).expect("source dir should exist");
        fs::create_dir_all(&cut_dir).expect("cut dir should exist");

        let client_id = Uuid::new_v4();
        let display_name = format!("display-{client_id}.bin");
        write_display_track(
            &source_dir.join(&display_name),
            &[
                stream_opened_frame(0),
                resize_frame(1_000, 215, 61),
                DisplayTrackEnvelope {
                    mono_ns: 30_000_000_000,
                    event: DisplayTrackEvent::FrameBytes { data: vec![b'x'] },
                },
                resize_frame(39_500_000_000, 120, 40),
                cursor_snapshot_frame(39_700_000_000, 118, 38),
                DisplayTrackEnvelope {
                    mono_ns: 40_000_000_000,
                    event: DisplayTrackEvent::StreamClosed,
                },
            ],
        );

        super::copy_display_tracks_for_cut(&source_dir, &cut_dir, 10)
            .expect("display track copy should succeed");

        let cut_frames = read_display_track(&cut_dir.join(display_name));
        let resize_values: Vec<(u16, u16)> = cut_frames
            .iter()
            .filter_map(|frame| match frame.event {
                DisplayTrackEvent::Resize { cols, rows } => Some((cols, rows)),
                _ => None,
            })
            .collect();
        assert_eq!(resize_values, vec![(120, 40)]);
    }

    #[test]
    fn rolling_cut_writes_completed_recording() {
        let root = temp_dir();
        let rolling_root = root.join(".rolling");
        let cut_root = root.join("cuts");

        let mut runtime = RecordingRuntime::new_rolling(rolling_root, 64, 300);
        runtime
            .start(
                None,
                true,
                None,
                RecordingProfile::Functional,
                vec![RecordingEventKind::PaneOutputRaw],
            )
            .expect("rolling recording should start");

        runtime
            .record(
                RecordingEventKind::PaneOutputRaw,
                RecordingPayload::Bytes {
                    data: b"hello".to_vec(),
                },
                RecordMeta {
                    session_id: None,
                    pane_id: None,
                    client_id: None,
                },
            )
            .expect("rolling event should be accepted");

        std::thread::sleep(Duration::from_millis(1200));

        let cut = runtime
            .cut(&cut_root, None, None)
            .expect("rolling cut should succeed");
        assert!(cut.ended_epoch_ms.is_some());
        assert!(cut.event_count >= 1);
        assert!(
            cut_root
                .join(cut.id.to_string())
                .join("manifest.json")
                .exists(),
            "cut manifest should exist"
        );

        let _ = runtime.stop(None).expect("rolling recording should stop");
    }

    #[test]
    fn rolling_cut_with_name_persists_name() {
        let root = temp_dir();
        let rolling_root = root.join(".rolling");
        let cut_root = root.join("cuts");

        let mut runtime = RecordingRuntime::new_rolling(rolling_root, 64, 300);
        runtime
            .start(
                None,
                true,
                None,
                RecordingProfile::Functional,
                vec![RecordingEventKind::PaneOutputRaw],
            )
            .expect("rolling recording should start");
        runtime
            .record(
                RecordingEventKind::PaneOutputRaw,
                RecordingPayload::Bytes {
                    data: b"hello".to_vec(),
                },
                RecordMeta {
                    session_id: None,
                    pane_id: None,
                    client_id: None,
                },
            )
            .expect("rolling event should be accepted");
        std::thread::sleep(Duration::from_millis(1200));

        let cut = runtime
            .cut(&cut_root, None, Some("look-at-this-one".to_string()))
            .expect("rolling cut should succeed");
        assert_eq!(cut.name.as_deref(), Some("look-at-this-one"));

        let _ = runtime.stop(None).expect("rolling recording should stop");
    }
}
