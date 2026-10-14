//! Private managed-runner adapter. Never acquires a terminal or installs signals.
//!
//! The owner is the playbook plugin. A retained control file contains sticky
//! cancellation and a monotonically increasing one-action permit count. Polling
//! never consumes commands, so a missed poll cannot lose stop or step intent.

use std::path::PathBuf;
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
}

#[derive(Debug, Serialize)]
struct Snapshot<'a> {
    generation: u64,
    phase: &'a str,
    step: usize,
    session_id: Option<String>,
    cleanup_acknowledged: bool,
    diagnostic: Option<&'a str>,
}

pub(super) struct ManagedRun {
    root: PathBuf,
    generation: u64,
    consumed_permits: u64,
}

impl ManagedRun {
    pub(super) fn from_env() -> Result<Option<Self>> {
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
            consumed_permits: 0,
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

    pub(super) fn publish_failure(&self, diagnostic: &str) -> Result<()> {
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

    pub(super) async fn permission(&mut self) -> Result<()> {
        loop {
            let control = self.control()?;
            ensure!(!control.stop, "managed playbook stopped");
            if !control.paused {
                // Old single-step permits cannot leak into a subsequent pause.
                self.consumed_permits = control.permits;
                return Ok(());
            }
            if control.permits > self.consumed_permits {
                self.consumed_permits += 1;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub(super) async fn cancelled(&self) {
        loop {
            match self.control() {
                Ok(control) if !control.stop => {}
                _ => return,
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub(super) fn publish(
        &self,
        phase: &str,
        step: usize,
        session_id: Option<uuid::Uuid>,
        cleanup_acknowledged: bool,
        diagnostic: Option<&str>,
    ) -> Result<()> {
        let diagnostic = diagnostic.map(|value| value.chars().take(2048).collect::<String>());
        let snapshot = Snapshot {
            generation: self.generation,
            phase,
            step,
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
    pub(super) async fn hold(&self) -> Result<()> {
        loop {
            let control = self.control()?;
            if control.stop || control.finish {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

pub(super) async fn cancellation(control: Option<&ManagedRun>) {
    if let Some(control) = control {
        control.cancelled().await;
    } else {
        std::future::pending::<()>().await;
    }
}

/// The managed process must use its own selected binary and paths, not deck
/// overrides or ambient runtime selectors. This is not a security sandbox:
/// explicitly trusted decks can execute arbitrary shell commands.
pub(super) fn validate_environment(playbook: &super::types::Playbook) -> Result<()> {
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
