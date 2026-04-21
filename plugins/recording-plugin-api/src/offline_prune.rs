//! Offline recording prune utility.
//!
//! Lives in the plugin-api rather than the plugin impl crate so the
//! CLI can prune the local recordings directory even when no bmux
//! server is running (e.g. `bmux recording prune` after a crash).
//! Shares the same manifest-file format the plugin writes but only
//! needs to parse the `ended_epoch_ms` timestamp, so we avoid pulling
//! in the full `RecordingRuntime` / manifest structure.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const MANIFEST_FILE_NAME: &str = "manifest.json";

/// Prune completed recordings older than the retention cutoff.
///
/// A recording is considered completed when its `manifest.json` has
/// an `ended_epoch_ms` field. Active (unended) recordings are never
/// pruned.
///
/// Returns the number of recording directories removed.
///
/// # Errors
///
/// Returns an error if the recordings root directory exists but
/// cannot be read.
pub fn prune_old_recordings(root_dir: &Path, retention_days: u64) -> anyhow::Result<usize> {
    if retention_days == 0 {
        return Ok(0);
    }
    if !root_dir.exists() {
        return Ok(0);
    }

    let cutoff_ms = epoch_millis_now().saturating_sub(retention_days * 24 * 60 * 60 * 1000);
    let mut deleted = 0;

    let entries = std::fs::read_dir(root_dir).map_err(|e| {
        anyhow::anyhow!("failed reading recordings dir {}: {e}", root_dir.display())
    })?;

    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join(MANIFEST_FILE_NAME);
        if !manifest_path.exists() {
            continue;
        }
        let Ok(bytes) = std::fs::read(&manifest_path) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        // Both wrapper `{ "summary": { "ended_epoch_ms": ... } }` and
        // flat-summary layouts have been seen historically; check
        // both.
        let ended_ms = value
            .get("summary")
            .and_then(|s| s.get("ended_epoch_ms"))
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                value
                    .get("ended_epoch_ms")
                    .and_then(serde_json::Value::as_u64)
            });
        if let Some(ended_ms) = ended_ms
            && ended_ms < cutoff_ms
        {
            if let Err(error) = std::fs::remove_dir_all(&path) {
                tracing::warn!("failed to prune recording {}: {error}", path.display());
                continue;
            }
            deleted += 1;
        }
    }

    Ok(deleted)
}

#[allow(clippy::cast_possible_truncation)]
fn epoch_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
