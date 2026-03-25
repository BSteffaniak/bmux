use anyhow::{Context, Result};
use bmux_ipc::{
    RecordingEventEnvelope, RecordingEventKind, RecordingPayload, RecordingStatus, RecordingSummary,
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
const EVENTS_FILE_NAME: &str = "events.jsonl";

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
    pub fn new(root_dir: PathBuf) -> Self {
        Self {
            root_dir,
            active: None,
        }
    }

    pub fn start(
        &mut self,
        session_filter: Option<Uuid>,
        capture_input: bool,
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
            session_id: session_filter,
            capture_input,
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
            session_id: active.session_filter,
            capture_input: active.capture_input,
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

    pub fn record(
        &self,
        kind: RecordingEventKind,
        payload: RecordingPayload,
        meta: RecordMeta,
    ) -> Result<()> {
        let Some(active) = self.active.as_ref() else {
            return Ok(());
        };

        if let Some(filter) = active.session_filter {
            if meta.session_id != Some(filter) {
                return Ok(());
            }
        }

        if matches!(kind, RecordingEventKind::PaneInputRaw) && !active.capture_input {
            return Ok(());
        }

        let seq = active.seq.fetch_add(1, Ordering::SeqCst).saturating_add(1);
        let envelope = RecordingEventEnvelope {
            seq,
            mono_ns: active
                .started_at
                .elapsed()
                .as_nanos()
                .min(u128::from(u64::MAX)) as u64,
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
            .map_err(|_| anyhow::anyhow!("recording writer is not accepting events"))
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
        serde_json::to_writer(&mut writer, &event)?;
        writer.write_all(b"\n")?;
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

fn payload_size(payload: &RecordingPayload) -> u64 {
    match payload {
        RecordingPayload::Bytes { data } => data.len() as u64,
        RecordingPayload::ServerEvent { event } => {
            serde_json::to_vec(event).map_or(0, |bytes| bytes.len() as u64)
        }
        RecordingPayload::RequestStart { request, .. } => request.len() as u64,
        RecordingPayload::RequestDone {
            request, response, ..
        } => (request.len() + response.len()) as u64,
        RecordingPayload::RequestError {
            request, message, ..
        } => (request.len() + message.len()) as u64,
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
    use bmux_ipc::{RecordingEventKind, RecordingPayload};
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
        let summary = runtime.start(None, true).expect("recording should start");
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
        runtime.start(None, false).expect("recording should start");
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
}
