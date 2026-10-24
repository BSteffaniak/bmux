//! Small observational shutdown counter. Only completed work advances it.
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Local observation, never authority to release resources or declare success.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownProgressSnapshot {
    pub version: u32,
    pub pid: u32,
    pub started_at_epoch_ms: u64,
    pub revision: u64,
    pub current_step: String,
}

#[derive(Default)]
struct State {
    publication: Option<(PathBuf, ShutdownProgressSnapshot)>,
    active: bool,
}

/// Shared by lifecycle owners; idle warnings and polls must never call advance.
#[derive(Clone, Default)]
pub struct ShutdownProgress(Arc<Mutex<State>>);
impl ShutdownProgress {
    /// Select the instance before shutdown. No file is published until begin.
    ///
    /// # Errors
    /// Reports poisoned state or configuration after shutdown has begun.
    pub fn configure(
        &self,
        path: PathBuf,
        pid: u32,
        started_at_epoch_ms: u64,
    ) -> Result<(), String> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| "shutdown progress lock poisoned")?;
        if state.active {
            return Err("shutdown progress already active".into());
        }
        state.publication = Some((
            path,
            ShutdownProgressSnapshot {
                version: 1,
                pid,
                started_at_epoch_ms,
                revision: 0,
                current_step: String::new(),
            },
        ));
        Ok(())
    }

    /// Begin once; repeated observers cannot manufacture progress.
    ///
    /// # Errors
    /// Reports poisoned state or publication failures.
    pub fn begin(&self) -> Result<(), String> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| "shutdown progress lock poisoned")?;
        if !state.active {
            state.active = true;
            publish(&mut state, "admission closed")?;
        }
        Ok(())
    }

    /// Report one real milestone, not a heartbeat, retry, or wait-loop iteration.
    /// Reports before begin are ignored (e.g. ordinary background saves).
    ///
    /// # Errors
    /// Reports poisoned state or publication failures. Owners must still clean up.
    pub fn advance(&self, step: &str) -> Result<(), String> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| "shutdown progress lock poisoned")?;
        if state.active {
            publish(&mut state, step)?;
        }
        Ok(())
    }
}

fn publish(state: &mut State, step: &str) -> Result<(), String> {
    let Some((path, snapshot)) = &mut state.publication else {
        return Ok(());
    };
    snapshot.revision = snapshot
        .revision
        .checked_add(1)
        .ok_or("shutdown progress counter exhausted")?;
    snapshot.current_step = step.chars().take(256).collect();
    // Serial publication under this lock prevents late revisions replacing newer
    // ones. No fsync: this is a replaceable observation, not a durable receipt.
    let staged = path.with_extension(format!("{}.next", snapshot.pid));
    let bytes = serde_json::to_vec(snapshot).map_err(|error| error.to_string())?;
    std::fs::write(&staged, bytes).map_err(|error| error.to_string())?;
    std::fs::rename(staged, path).map_err(|error| error.to_string())
}

/// CLI-local inactivity clock; wall clocks and file mtimes are not consulted.
pub struct ShutdownIdleTimer {
    revision: u64,
    last_progress: Instant,
    idle: Duration,
    step: Option<String>,
}
impl ShutdownIdleTimer {
    #[must_use]
    pub const fn new(now: Instant, idle: Duration) -> Self {
        Self {
            revision: 0,
            last_progress: now,
            idle,
            step: None,
        }
    }
    /// Only a newer, supported observation from the captured instance counts.
    ///
    /// # Errors
    /// Rejects unsupported matching schemas and malformed steps.
    pub fn observe(
        &mut self,
        value: &ShutdownProgressSnapshot,
        pid: u32,
        started_at_epoch_ms: u64,
        now: Instant,
    ) -> Result<(), String> {
        if value.pid != pid || value.started_at_epoch_ms != started_at_epoch_ms {
            return Ok(());
        }
        if value.version != 1 {
            return Err(format!(
                "unsupported shutdown progress version {}",
                value.version
            ));
        }
        if value.current_step.len() > 1024 {
            return Err("oversized shutdown progress step".into());
        }
        if value.revision > self.revision {
            self.revision = value.revision;
            self.last_progress = now;
            self.step = Some(value.current_step.clone());
        }
        Ok(())
    }
    #[must_use]
    pub fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.last_progress) >= self.idle
    }
    #[must_use]
    pub fn last_step(&self) -> &str {
        self.step
            .as_deref()
            .unwrap_or("progress unavailable (older server or shutdown has not started)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(revision: u64) -> ShutdownProgressSnapshot {
        ShutdownProgressSnapshot {
            version: 1,
            pid: 42,
            started_at_epoch_ms: 99,
            revision,
            current_step: "work completed".into(),
        }
    }
    #[test]
    fn progress_can_extend_wait_indefinitely_but_repeated_or_stale_reports_cannot() {
        let start = Instant::now();
        let mut timer = ShutdownIdleTimer::new(start, Duration::from_secs(30));
        for revision in 1..=10 {
            let now = start + Duration::from_secs(revision * 20);
            timer.observe(&snapshot(revision), 42, 99, now).unwrap();
            assert!(!timer.expired(now));
        }
        let now = start + Duration::from_secs(230);
        timer.observe(&snapshot(10), 42, 99, now).unwrap();
        timer.observe(&snapshot(11), 42, 100, now).unwrap();
        timer.observe(&snapshot(11), 43, 99, now).unwrap();
        timer.observe(&snapshot(9), 42, 99, now).unwrap();
        assert!(timer.expired(now));
        timer.observe(&snapshot(11), 42, 99, now).unwrap();
        assert!(!timer.expired(now));
    }
    #[test]
    fn missing_progress_expires_and_unknown_schema_is_explicit() {
        let start = Instant::now();
        let mut timer = ShutdownIdleTimer::new(start, Duration::from_secs(30));
        assert!(timer.expired(start + Duration::from_secs(30)));
        assert!(timer.last_step().contains("unavailable"));
        let mut value = snapshot(1);
        value.version = 2;
        assert!(timer.observe(&value, 42, 99, start).is_err());
    }
}
