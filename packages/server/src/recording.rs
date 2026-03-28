use anyhow::{Context, Result};
use bmux_ipc::{
    RecordingEventEnvelope, RecordingEventKind, RecordingPayload, RecordingProfile,
    RecordingStatus, RecordingSummary,
};
use serde::{Deserialize, Serialize};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const MANIFEST_FILE_NAME: &str = "manifest.json";
const EVENTS_FILE_NAME: &str = "events.bin";

#[derive(Debug)]
pub struct RecordingRuntime {
    root_dir: PathBuf,
    active: Option<ActiveRecording>,
}

#[derive(Debug)]
struct ActiveRecording {
    id: Uuid,
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
    pub const fn new(root_dir: PathBuf) -> Self {
        Self {
            root_dir,
            active: None,
        }
    }

    pub fn start(
        &mut self,
        session_filter: Option<Uuid>,
        capture_input: bool,
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
        let started_epoch_ms = epoch_millis_now();
        let dir = self.root_dir.join(id.to_string());
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed creating recording dir {}", dir.display()))?;

        let manifest_path = dir.join(MANIFEST_FILE_NAME);
        let events_path = dir.join(EVENTS_FILE_NAME);
        let summary = RecordingSummary {
            id,
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
        };
        write_manifest(&manifest_path, &summary)?;

        let (tx, rx) = mpsc::channel::<RecordingEventEnvelope>();
        let event_count = Arc::new(AtomicU64::new(0));
        let payload_bytes = Arc::new(AtomicU64::new(0));
        let event_count_thread = Arc::clone(&event_count);
        let payload_bytes_thread = Arc::clone(&payload_bytes);
        let summary_for_thread = summary.clone();

        let writer = thread::Builder::new()
            .name(format!("bmux-recording-{id}"))
            .spawn(move || {
                writer_loop(
                    rx,
                    &events_path,
                    &manifest_path,
                    summary_for_thread,
                    event_count_thread,
                    payload_bytes_thread,
                )
            })
            .context("failed to spawn recording writer thread")?;

        self.active = Some(ActiveRecording {
            id,
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
        });
        RecordingStatus {
            active,
            queue_len: 0,
        }
    }

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
}

fn writer_loop(
    rx: mpsc::Receiver<RecordingEventEnvelope>,
    events_path: &Path,
    manifest_path: &Path,
    mut summary: RecordingSummary,
    event_count: Arc<AtomicU64>,
    payload_bytes: Arc<AtomicU64>,
) -> Result<RecordingSummary> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(events_path)
        .with_context(|| {
            format!(
                "failed opening recording event file {}",
                events_path.display()
            )
        })?;
    let mut writer = BufWriter::new(file);

    while let Ok(event) = rx.recv() {
        bmux_ipc::write_frame(&mut writer, &event)
            .map_err(|e| anyhow::anyhow!("recording write_frame failed: {e}"))?;
        let payload_size = payload_size(&event.payload);
        event_count.fetch_add(1, Ordering::SeqCst);
        payload_bytes.fetch_add(payload_size, Ordering::SeqCst);
        if event.seq % 128 == 0 {
            writer.flush()?;
            summary.event_count = event_count.load(Ordering::SeqCst);
            summary.payload_bytes = payload_bytes.load(Ordering::SeqCst);
            write_manifest(manifest_path, &summary)?;
        }
    }

    writer.flush()?;
    summary.event_count = event_count.load(Ordering::SeqCst);
    summary.payload_bytes = payload_bytes.load(Ordering::SeqCst);
    summary.ended_epoch_ms = Some(epoch_millis_now());
    write_manifest(manifest_path, &summary)?;
    Ok(summary)
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
/// and postcard envelope overhead). Treat `payload_bytes` in the manifest as
/// an approximate content-size metric, not an exact file-size measurement.
fn payload_size(payload: &RecordingPayload) -> u64 {
    match payload {
        RecordingPayload::Bytes { data } => data.len() as u64,
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

fn epoch_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::{RecordMeta, RecordingRuntime};
    use bmux_ipc::{RecordingEventKind, RecordingPayload, RecordingProfile};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
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

    #[test]
    fn start_record_stop_persists_manifest() {
        let root = temp_dir();
        let mut runtime = RecordingRuntime::new(root.clone());
        let summary = runtime
            .start(
                None,
                true,
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
    fn no_capture_input_suppresses_input_events() {
        let root = temp_dir();
        let mut runtime = RecordingRuntime::new(root);
        runtime
            .start(
                None,
                false,
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
        let mut runtime = RecordingRuntime::new(root.clone());
        let summary = runtime
            .start(
                None,
                true,
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
        let mut runtime = RecordingRuntime::new(root);
        let active = runtime
            .start(
                None,
                true,
                RecordingProfile::Functional,
                vec![RecordingEventKind::PaneOutputRaw],
            )
            .expect("first recording should start");
        let second = runtime
            .start(
                None,
                true,
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
                RecordingProfile::Functional,
                vec![RecordingEventKind::PaneOutputRaw],
            )
            .expect("new active should start");

        let deleted_count = runtime.delete_all().expect("delete all should succeed");
        assert_eq!(deleted_count, 2);
        assert!(runtime.status().active.is_none());
        assert!(runtime.list().expect("list should succeed").is_empty());
    }
}
