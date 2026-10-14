//! Private managed-runner adapter. Never acquires a terminal or installs signals.
//!
//! The owner is the playbook plugin. A retained control file contains sticky
//! cancellation and a monotonically increasing one-action permit count. Polling
//! never consumes commands, so a missed poll cannot lose stop or step intent.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize)]
struct Control {
    generation: u64,
    paused: bool,
    permits: u64,
    stop: bool,
    finish: bool,
    #[serde(default)]
    playback_speed: String,
}

#[derive(Debug, Serialize)]
struct Snapshot<'a> {
    generation: u64,
    phase: &'a str,
    step: usize,
    playback_speed: &'a str,
    current_action: &'a str,
    action_count: usize,
    session_id: Option<String>,
    cleanup_acknowledged: bool,
    diagnostic: Option<&'a str>,
}

pub struct ManagedRun {
    root: PathBuf,
    generation: u64,
    consumed_permits: AtomicU64,
    stepping: AtomicBool,
    current_action: &'static str,
    action_count: usize,
}

impl ManagedRun {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(root) = std::env::var_os("BMUX_PLAYBOOK_MANAGED_ROOT") else {
            return Ok(None);
        };
        let root = PathBuf::from(root);
        ensure!(root.is_absolute(), "managed root must be absolute");
        let generation = std::env::var("BMUX_PLAYBOOK_MANAGED_GENERATION")?
            .parse()
            .context("invalid managed generation")?;
        Ok(Some(Self {
            root,
            generation,
            consumed_permits: AtomicU64::new(0),
            stepping: AtomicBool::new(false),
            current_action: "",
            action_count: 0,
        }))
    }

    fn control(&self) -> Result<Control> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(self.root.join("control.json"))?
            .take(4097)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 4096, "oversized managed control");
        let control: Control = serde_json::from_slice(&bytes)?;
        ensure!(
            control.generation == self.generation,
            "stale managed generation"
        );
        Ok(control)
    }

    pub fn publish_failure(&self, diagnostic: &str) -> Result<()> {
        // Marker is set BEFORE spawning and removed only after spawn rejection
        // or successful wait/reaping. Absence is therefore positive ownership
        // evidence in this worker, not a guess based on runtime unreachability.
        let acknowledged = !self.root.join("sandbox-child-owned").try_exists()?;
        self.publish(
            if acknowledged {
                "failed"
            } else {
                "cleanup-failed"
            },
            0,
            None,
            acknowledged,
            Some(diagnostic),
        )
    }

    pub fn action(&mut self, label: &'static str, count: usize) {
        self.current_action = label;
        self.action_count = count;
        self.stepping.store(false, Ordering::Relaxed);
    }

    pub fn speed(&self) -> Result<crate::PlaybackSpeed> {
        let control = self.control()?;
        ensure!(!control.stop, "managed playbook stopped");
        if control.playback_speed.is_empty() {
            return Ok(crate::PlaybackSpeed::Instant);
        }
        crate::PlaybackSpeed::parse(&control.playback_speed)
            .context("invalid managed playback speed")
    }

    pub async fn permission(&mut self) -> Result<()> {
        self.checkpoint().await
    }

    pub async fn checkpoint(&self) -> Result<()> {
        loop {
            let control = self.control()?;
            ensure!(!control.stop, "managed playbook stopped");
            if !control.paused {
                self.consumed_permits
                    .store(control.permits, Ordering::Relaxed);
                self.stepping.store(false, Ordering::Relaxed);
                return Ok(());
            }
            if self.stepping.load(Ordering::Relaxed) {
                return Ok(());
            }
            if control.permits > self.consumed_permits.load(Ordering::Relaxed) {
                self.consumed_permits.fetch_add(1, Ordering::Relaxed);
                self.stepping.store(true, Ordering::Relaxed);
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn cancelled(&self) {
        loop {
            match self.control() {
                Ok(control) if !control.stop => {}
                _ => return,
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub fn publish(
        &self,
        phase: &str,
        step: usize,
        session_id: Option<uuid::Uuid>,
        cleanup_acknowledged: bool,
        diagnostic: Option<&str>,
    ) -> Result<()> {
        let diagnostic = diagnostic.map(|value| value.chars().take(2048).collect::<String>());
        let speed = self.control()?.playback_speed;
        let snapshot = Snapshot {
            generation: self.generation,
            phase,
            step,
            playback_speed: &speed,
            current_action: self.current_action,
            action_count: self.action_count,
            session_id: session_id.map(|id| id.to_string()),
            cleanup_acknowledged,
            diagnostic: diagnostic.as_deref(),
        };
        let pending = self.root.join("runner.pending");
        std::fs::write(&pending, serde_json::to_vec(&snapshot)?)?;
        std::fs::rename(pending, self.root.join("runner.json"))?;
        Ok(())
    }

    /// Keep the actual sandbox alive after script completion for presentation.
    /// Finishing or cancelling is explicit; there is no fake seek/replay.
    pub async fn hold(&self) -> Result<()> {
        loop {
            let control = self.control()?;
            if control.stop || control.finish {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

pub async fn cancellation(control: Option<&ManagedRun>) {
    if let Some(control) = control {
        control.cancelled().await;
    } else {
        std::future::pending::<()>().await;
    }
}

/// Managed decks cannot replace the selected runtime environment.
pub fn validate_environment(playbook: &super::types::Playbook) -> Result<()> {
    for key in playbook.config.env.keys() {
        if key.starts_with("BMUX_") || key.starts_with("XDG_") {
            bail!("managed playbook cannot override runtime selector {key}");
        }
    }
    ensure!(
        playbook.config.sandbox_config_file.is_none(),
        "managed execution requires generated isolated configuration"
    );
    ensure!(
        matches!(
            playbook.config.driver,
            super::types::PlaybookDriver::Sandbox
        ),
        "managed execution uses the real sandbox action driver; CLI attach simulation/interactive drivers are not embedded"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pacing;
    use std::sync::atomic::AtomicU64;

    fn fixture() -> (tempfile::TempDir, ManagedRun) {
        let root = tempfile::tempdir().unwrap();
        let run = ManagedRun {
            root: root.path().into(),
            generation: 7,
            consumed_permits: AtomicU64::new(0),
            stepping: AtomicBool::new(false),
            current_action: "send-keys",
            action_count: 1,
        };
        write_control(&run, false, 0, false, 7, "normal");
        (root, run)
    }

    fn write_control(
        run: &ManagedRun,
        paused: bool,
        permits: u64,
        stop: bool,
        generation: u64,
        speed: &str,
    ) {
        let value = serde_json::json!({
            "generation": generation, "paused": paused, "permits": permits,
            "stop": stop, "finish": false, "playback_speed": speed,
        });
        std::fs::write(
            run.root.join("control.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn partial_typing_pause_resume_never_replays_or_enters_early() {
        let (_root, mut run) = fixture();
        run.action("send-keys", 1);
        run.permission().await.unwrap();
        let payload = "printf '👩‍💻e\u{301}'\r".as_bytes();
        let chunks = pacing::chunks(payload);
        assert!(chunks.len() > 3);
        assert_eq!(chunks.last().copied(), Some(b"\r".as_slice()));
        let mut sent = chunks[0].to_vec();
        write_control(&run, true, 0, false, 7, "normal");
        let credit = AtomicU64::new(0);
        let delay = pacing::wait(Some(&run), crate::PlaybackSpeed::Instant, true, &credit);
        tokio::pin!(delay);
        tokio::select! {
            result = &mut delay => panic!("paused typing advanced: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        assert_eq!(sent, chunks[0]);
        assert_eq!(pacing::overhead(&credit), Duration::ZERO);
        write_control(&run, false, 0, false, 7, "double");
        delay.await.unwrap();
        for chunk in &chunks[1..] {
            run.checkpoint().await.unwrap();
            sent.extend_from_slice(chunk);
        }
        assert_eq!(sent, payload);
    }

    #[tokio::test(start_paused = true)]
    async fn step_permit_covers_whole_action_not_each_chunk() {
        let (_root, mut run) = fixture();
        write_control(&run, true, 1, false, 7, "normal");
        run.action("send-keys", 2);
        run.permission().await.unwrap();
        let credit = AtomicU64::new(0);
        for _ in 0..3 {
            pacing::wait(Some(&run), crate::PlaybackSpeed::Instant, true, &credit)
                .await
                .unwrap();
        }
        run.action("send-keys", 2);
        let next = run.permission();
        tokio::pin!(next);
        tokio::select! {
            result = &mut next => panic!("permit leaked into next action: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stop_and_generation_change_cancel_retained_partial_action() {
        let (_root, run) = fixture();
        let credit = AtomicU64::new(0);
        let delay = pacing::wait(Some(&run), crate::PlaybackSpeed::Instant, true, &credit);
        tokio::pin!(delay);
        tokio::select! {
            result = &mut delay => panic!("delay completed early: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        write_control(&run, false, 0, true, 7, "normal");
        assert!(delay.await.is_err());
        write_control(&run, false, 0, false, 8, "normal");
        assert!(run.checkpoint().await.is_err());
        assert!(run.speed().is_err());
    }
}
