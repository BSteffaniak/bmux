#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Domain-agnostic segmented diagnostic log writer.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const MANIFEST_FILE_NAME: &str = "manifest.json";
const LATEST_LINK_NAME: &str = "latest";
const LATEST_LOG_LINK_NAME: &str = "latest.log";
const FORMAT_VERSION: u32 = 1;

/// Diagnostic log layout mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticLogMode {
    /// Write rotated segments under a per-process run directory.
    Segmented,
    /// Write all output into one append-only file.
    Unified,
    /// Disable this log sink.
    Off,
}

/// Configuration for one diagnostic log sink.
#[derive(Debug, Clone)]
pub struct DiagnosticLogConfig {
    /// Root directory for this scope. Segmented logs create `runs/` here;
    /// unified logs write directly here.
    pub root_dir: PathBuf,
    /// Short scope name, for example `server` or `client`.
    pub kind: String,
    /// Filename prefix for segment/unified files.
    pub file_prefix: String,
    /// Stable identifier for this run.
    pub run_id: String,
    /// Logging mode.
    pub mode: DiagnosticLogMode,
    /// Segment rotation size in MiB. `0` disables size rotation.
    pub segment_mb: usize,
    /// Completed run/file retention in days. `0` keeps indefinitely.
    pub retention_days: u64,
    /// Maximum total bytes under this scope. `0` disables size pruning.
    pub max_total_mb: usize,
    /// Optional client/process identifier to write into the manifest.
    pub client_id: Option<String>,
}

/// On-disk manifest for segmented diagnostic logs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiagnosticLogManifest {
    pub format_version: u32,
    pub kind: String,
    pub run_id: String,
    pub pid: u32,
    pub client_id: Option<String>,
    pub started_epoch_ms: u64,
    pub ended_epoch_ms: Option<u64>,
    pub segments: Vec<String>,
    pub total_segment_bytes: u64,
    pub dropped_events: u64,
}

/// Shared writer suitable for tracing sinks.
#[derive(Clone, Debug)]
pub struct DiagnosticLogWriter {
    inner: Arc<Mutex<DiagnosticLogState>>,
}

/// Handle that finalizes the manifest when dropped.
#[derive(Debug)]
pub struct DiagnosticLogHandle {
    inner: Arc<Mutex<DiagnosticLogState>>,
}

#[derive(Debug)]
struct DiagnosticLogState {
    mode: DiagnosticLogMode,
    root_dir: PathBuf,
    run_dir: Option<PathBuf>,
    manifest_path: Option<PathBuf>,
    file_prefix: String,
    segment_limit_bytes: u64,
    segment_index: usize,
    segment_bytes: u64,
    manifest: Option<DiagnosticLogManifest>,
    writer: Option<BufWriter<File>>,
}

/// Errors from diagnostic log setup.
#[derive(Debug, thiserror::Error)]
pub enum DiagnosticLogError {
    #[error("failed creating log directory {path}")]
    CreateDir { path: String, source: io::Error },
    #[error("failed opening log file {path}")]
    OpenFile { path: String, source: io::Error },
    #[error("failed writing manifest {path}")]
    WriteManifest { path: String, source: io::Error },
    #[error("failed serializing manifest")]
    SerializeManifest(#[source] serde_json::Error),
}

impl DiagnosticLogWriter {
    /// Create a writer and lifecycle handle for the configured sink.
    ///
    /// # Errors
    ///
    /// Returns an error if directories, files, or the initial manifest cannot be created.
    pub fn start(
        config: DiagnosticLogConfig,
    ) -> Result<(Self, DiagnosticLogHandle), DiagnosticLogError> {
        let state = match config.mode {
            DiagnosticLogMode::Off => DiagnosticLogState::off(config),
            DiagnosticLogMode::Unified => DiagnosticLogState::start_unified(config)?,
            DiagnosticLogMode::Segmented => DiagnosticLogState::start_segmented(config)?,
        };
        let inner = Arc::new(Mutex::new(state));
        Ok((
            Self {
                inner: Arc::clone(&inner),
            },
            DiagnosticLogHandle { inner },
        ))
    }
}

impl DiagnosticLogHandle {
    /// Update the manifest client identifier.
    pub fn set_client_id(&self, client_id: impl Into<String>) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.set_client_id(Some(client_id.into()));
        }
    }

    /// Return the active log path for this handle.
    #[must_use]
    pub fn active_path(&self) -> Option<PathBuf> {
        self.inner.lock().ok().and_then(|guard| guard.active_path())
    }
}

impl Drop for DiagnosticLogHandle {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.finish();
        }
    }
}

impl Write for DiagnosticLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("diagnostic log writer lock poisoned"))?;
        guard.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("diagnostic log writer lock poisoned"))?;
        guard.flush()
    }
}

impl DiagnosticLogState {
    fn off(config: DiagnosticLogConfig) -> Self {
        Self {
            mode: DiagnosticLogMode::Off,
            root_dir: config.root_dir,
            run_dir: None,
            manifest_path: None,
            file_prefix: config.file_prefix,
            segment_limit_bytes: mb_to_bytes(config.segment_mb),
            segment_index: 0,
            segment_bytes: 0,
            manifest: None,
            writer: None,
        }
    }

    fn start_unified(config: DiagnosticLogConfig) -> Result<Self, DiagnosticLogError> {
        create_dir_all(&config.root_dir)?;
        let file_path = config.root_dir.join(format!("{}.log", config.file_prefix));
        let writer = open_append(&file_path)?;
        Ok(Self {
            mode: DiagnosticLogMode::Unified,
            root_dir: config.root_dir,
            run_dir: None,
            manifest_path: None,
            file_prefix: config.file_prefix,
            segment_limit_bytes: mb_to_bytes(config.segment_mb),
            segment_index: 0,
            segment_bytes: file_path.metadata().map_or(0, |metadata| metadata.len()),
            manifest: None,
            writer: Some(writer),
        })
    }

    fn start_segmented(config: DiagnosticLogConfig) -> Result<Self, DiagnosticLogError> {
        let run_parent_dir = config.root_dir.join("runs");
        let run_dir = run_parent_dir.join(&config.run_id);
        create_dir_all(&run_dir)?;
        let manifest_path = run_dir.join(MANIFEST_FILE_NAME);
        let mut state = Self {
            mode: DiagnosticLogMode::Segmented,
            root_dir: config.root_dir.clone(),
            run_dir: Some(run_dir.clone()),
            manifest_path: Some(manifest_path),
            file_prefix: config.file_prefix,
            segment_limit_bytes: mb_to_bytes(config.segment_mb),
            segment_index: 0,
            segment_bytes: 0,
            manifest: Some(DiagnosticLogManifest {
                format_version: FORMAT_VERSION,
                kind: config.kind,
                run_id: config.run_id,
                pid: std::process::id(),
                client_id: config.client_id,
                started_epoch_ms: now_epoch_ms(),
                ended_epoch_ms: None,
                segments: Vec::new(),
                total_segment_bytes: 0,
                dropped_events: 0,
            }),
            writer: None,
        };
        state.open_next_segment()?;
        state.write_manifest()?;
        replace_latest_link(&config.root_dir.join(LATEST_LINK_NAME), &run_dir);
        state.prune(config.retention_days, config.max_total_mb);
        Ok(state)
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.mode {
            DiagnosticLogMode::Off => Ok(buf.len()),
            DiagnosticLogMode::Unified => {
                if let Some(writer) = self.writer.as_mut() {
                    writer.write_all(buf)?;
                    self.segment_bytes = self
                        .segment_bytes
                        .saturating_add(u64::try_from(buf.len()).unwrap_or(u64::MAX));
                }
                Ok(buf.len())
            }
            DiagnosticLogMode::Segmented => {
                if self.should_rotate(buf.len()) {
                    self.open_next_segment()
                        .map_err(|error| io::Error::other(error.to_string()))?;
                }
                if let Some(writer) = self.writer.as_mut() {
                    writer.write_all(buf)?;
                    let written = u64::try_from(buf.len()).unwrap_or(u64::MAX);
                    self.segment_bytes = self.segment_bytes.saturating_add(written);
                    if let Some(manifest) = self.manifest.as_mut() {
                        manifest.total_segment_bytes =
                            manifest.total_segment_bytes.saturating_add(written);
                    }
                }
                Ok(buf.len())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.flush()?;
        }
        Ok(())
    }

    fn should_rotate(&self, incoming_len: usize) -> bool {
        self.segment_limit_bytes > 0
            && self.segment_bytes > 0
            && self
                .segment_bytes
                .saturating_add(u64::try_from(incoming_len).unwrap_or(u64::MAX))
                > self.segment_limit_bytes
    }

    fn open_next_segment(&mut self) -> Result<(), DiagnosticLogError> {
        if let Some(writer) = self.writer.as_mut() {
            let _ = writer.flush();
        }
        let Some(run_dir) = self.run_dir.as_ref() else {
            return Ok(());
        };
        let file_name = format!("{}_{:06}.log", self.file_prefix, self.segment_index);
        self.segment_index = self.segment_index.saturating_add(1);
        let file_path = run_dir.join(&file_name);
        self.writer = Some(open_append(&file_path)?);
        self.segment_bytes = file_path.metadata().map_or(0, |metadata| metadata.len());
        if let Some(manifest) = self.manifest.as_mut() {
            manifest.segments.push(file_name);
        }
        replace_latest_link(&run_dir.join(LATEST_LOG_LINK_NAME), &file_path);
        self.write_manifest()?;
        Ok(())
    }

    fn set_client_id(&mut self, client_id: Option<String>) {
        if let Some(manifest) = self.manifest.as_mut() {
            manifest.client_id = client_id;
            let _ = self.write_manifest();
        }
    }

    fn finish(&mut self) {
        if let Some(writer) = self.writer.as_mut() {
            let _ = writer.flush();
        }
        if let Some(manifest) = self.manifest.as_mut() {
            manifest.ended_epoch_ms = Some(now_epoch_ms());
            let _ = self.write_manifest();
        }
    }

    fn active_path(&self) -> Option<PathBuf> {
        match self.mode {
            DiagnosticLogMode::Off => None,
            DiagnosticLogMode::Unified => {
                Some(self.root_dir.join(format!("{}.log", self.file_prefix)))
            }
            DiagnosticLogMode::Segmented => self.run_dir.clone(),
        }
    }

    fn write_manifest(&self) -> Result<(), DiagnosticLogError> {
        let Some(manifest_path) = self.manifest_path.as_ref() else {
            return Ok(());
        };
        let Some(manifest) = self.manifest.as_ref() else {
            return Ok(());
        };
        let bytes =
            serde_json::to_vec_pretty(manifest).map_err(DiagnosticLogError::SerializeManifest)?;
        std::fs::write(manifest_path, bytes).map_err(|source| DiagnosticLogError::WriteManifest {
            path: manifest_path.display().to_string(),
            source,
        })
    }

    fn prune(&self, retention_days: u64, max_total_mb: usize) {
        if retention_days == 0 && max_total_mb == 0 {
            return;
        }
        let runs_dir = self.root_dir.join("runs");
        let active_run_dir = self.run_dir.as_ref();
        let mut candidates = collect_run_candidates(&runs_dir, active_run_dir);
        let now_ms = now_epoch_ms();
        if retention_days > 0 {
            let retention_ms = retention_days.saturating_mul(24 * 60 * 60 * 1000);
            for candidate in &candidates {
                if now_ms.saturating_sub(candidate.modified_epoch_ms) > retention_ms {
                    let _ = std::fs::remove_dir_all(&candidate.path);
                }
            }
            candidates.retain(|candidate| candidate.path.exists());
        }
        let max_total_bytes = mb_to_bytes(max_total_mb);
        if max_total_bytes == 0 {
            return;
        }
        let mut total = candidates
            .iter()
            .fold(0_u64, |acc, candidate| acc.saturating_add(candidate.bytes));
        while total > max_total_bytes {
            let Some(candidate) = candidates.pop_front() else {
                break;
            };
            total = total.saturating_sub(candidate.bytes);
            let _ = std::fs::remove_dir_all(candidate.path);
        }
    }
}

#[derive(Debug)]
struct PruneCandidate {
    path: PathBuf,
    modified_epoch_ms: u64,
    bytes: u64,
}

fn collect_run_candidates(
    runs_dir: &Path,
    active_run_dir: Option<&PathBuf>,
) -> VecDeque<PruneCandidate> {
    let Ok(entries) = std::fs::read_dir(runs_dir) else {
        return VecDeque::new();
    };
    let mut candidates = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| active_run_dir != Some(path))
        .filter_map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            let modified_epoch_ms = metadata
                .modified()
                .ok()
                .and_then(system_time_epoch_ms)
                .unwrap_or(0);
            Some(PruneCandidate {
                bytes: dir_size(&path),
                path,
                modified_epoch_ms,
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| candidate.modified_epoch_ms);
    candidates.into()
}

fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries.filter_map(Result::ok).fold(0_u64, |acc, entry| {
        let path = entry.path();
        let bytes = if path.is_dir() {
            dir_size(&path)
        } else {
            entry.metadata().map_or(0, |metadata| metadata.len())
        };
        acc.saturating_add(bytes)
    })
}

fn open_append(path: &Path) -> Result<BufWriter<File>, DiagnosticLogError> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(BufWriter::new)
        .map_err(|source| DiagnosticLogError::OpenFile {
            path: path.display().to_string(),
            source,
        })
}

fn create_dir_all(path: &Path) -> Result<(), DiagnosticLogError> {
    std::fs::create_dir_all(path).map_err(|source| DiagnosticLogError::CreateDir {
        path: path.display().to_string(),
        source,
    })
}

fn mb_to_bytes(mb: usize) -> u64 {
    u64::try_from(mb)
        .unwrap_or(u64::MAX)
        .saturating_mul(1024 * 1024)
}

fn now_epoch_ms() -> u64 {
    system_time_epoch_ms(SystemTime::now()).unwrap_or(0)
}

fn system_time_epoch_ms(time: SystemTime) -> Option<u64> {
    let duration = time.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(duration.as_millis()).ok()
}

#[cfg(unix)]
fn replace_latest_link(link_path: &Path, target_path: &Path) {
    let _ = std::fs::remove_file(link_path);
    let _ = std::os::unix::fs::symlink(target_path, link_path);
}

#[cfg(windows)]
fn replace_latest_link(link_path: &Path, target_path: &Path) {
    let _ = std::fs::remove_dir(link_path);
    let _ = std::os::windows::fs::symlink_dir(target_path, link_path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(root_dir: PathBuf) -> DiagnosticLogConfig {
        DiagnosticLogConfig {
            root_dir,
            kind: "test".to_owned(),
            file_prefix: "test".to_owned(),
            run_id: "run-1".to_owned(),
            mode: DiagnosticLogMode::Segmented,
            segment_mb: 1,
            retention_days: 0,
            max_total_mb: 0,
            client_id: None,
        }
    }

    #[test]
    fn writes_segment_manifest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let (mut writer, handle) =
            DiagnosticLogWriter::start(test_config(temp.path().to_path_buf()))
                .expect("start writer");
        writer.write_all(b"hello\n").expect("write log");
        writer.flush().expect("flush log");
        drop(handle);

        let run_dir = temp.path().join("runs/run-1");
        assert!(run_dir.join("test_000000.log").exists());
        let manifest: DiagnosticLogManifest = serde_json::from_slice(
            &std::fs::read(run_dir.join(MANIFEST_FILE_NAME)).expect("read manifest"),
        )
        .expect("parse manifest");
        assert_eq!(manifest.segments, vec!["test_000000.log"]);
        assert!(manifest.ended_epoch_ms.is_some());
    }

    #[test]
    fn unified_mode_writes_single_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = test_config(temp.path().to_path_buf());
        config.mode = DiagnosticLogMode::Unified;
        let (mut writer, _handle) = DiagnosticLogWriter::start(config).expect("start writer");
        writer.write_all(b"hello\n").expect("write log");
        writer.flush().expect("flush log");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("test.log")).expect("read log"),
            "hello\n"
        );
    }
}
