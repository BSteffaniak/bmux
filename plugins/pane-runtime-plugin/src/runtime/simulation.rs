//! Driving the pane runtime from a deterministic simulation (the `simulator` feature).
//!
//! [`Panes`] is a pane runtime with no server around it: it opens panes running the configured
//! shell on the simulated kernel, types into them, resizes and closes them, and reports what a
//! simulation checks (each pane's process, whether it exited, whether closing drained its
//! output and cleaned up). Everything underneath is the real pane runtime: its tasks, threads,
//! terminals and process control, all on the simulation's executor, threads, clock and kernel.

use bmux_session_models::SessionId;
use switchy_thread::sync::{Arc, atomic::Ordering};
use tokio::sync::mpsc;

use super::{
    PaneExitEvent, PanePaddingConfig, PaneRuntimeCommand, PaneRuntimeHandle, PaneRuntimeHost,
    SessionRuntimeManager, protocol_profile_for_term, shutdown_runtime_handle,
};

/// Run the platform layer's conformance scenarios against the simulated target, in the current
/// simulation run (whose kernel's personality decides what each expects): each scenario's
/// name, and what went wrong.
#[must_use]
pub fn platform_conformance() -> Vec<(&'static str, Result<(), String>)> {
    crate::platform::conformance::SCENARIOS
        .iter()
        .map(|(name, scenario)| (*name, scenario()))
        .collect()
}

/// A pane runtime under simulation.
pub struct Panes {
    manager: SessionRuntimeManager,
    host: PaneRuntimeHost,
    exits: mpsc::UnboundedReceiver<PaneExitEvent>,
}

/// One pane (each in a session of its own).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pane(SessionId);

/// How closing a pane went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closed {
    /// Its output was drained before the pane ended.
    pub drained: bool,
    /// Cleaning up its process or terminal failed: the first error, if so.
    pub cleanup_error: Option<String>,
}

impl std::fmt::Debug for Panes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Panes")
            .field("panes", &self.manager.runtimes.len())
            .finish_non_exhaustive()
    }
}

impl Panes {
    /// A pane runtime whose panes run `shell` (a program registered with the simulated
    /// kernel, by its base name).
    #[must_use]
    pub fn new(shell: &str) -> Self {
        Self::build(shell, None)
    }

    /// As [`Self::new`], recording each pane's session under `records` (as an activation
    /// with a runtime directory does), after sweeping the sessions a crashed predecessor
    /// recorded there.
    #[must_use]
    pub fn recovering(shell: &str, records: &std::path::Path) -> Self {
        Self::build(shell, Some(records))
    }

    fn build(shell: &str, records: Option<&std::path::Path>) -> Self {
        let (exit_tx, exits) = mpsc::unbounded_channel();
        let host = PaneRuntimeHost::detached();
        let manager = SessionRuntimeManager::new(
            host.clone(),
            shell.into(),
            "xterm-256color".into(),
            protocol_profile_for_term("xterm-256color"),
            true,
            None,
            PanePaddingConfig::default(),
            exit_tx,
        );
        if let Some(records) = records {
            host.sessions().install_records(records.to_path_buf());
        }
        Self {
            manager,
            host,
            exits,
        }
    }

    /// Open a pane of `rows` by `cols`, once its program is ready.
    ///
    /// # Errors
    ///
    /// * If the pane cannot be prepared, started or committed
    pub async fn open(&mut self, rows: u16, cols: u16) -> anyhow::Result<Pane> {
        let session_id = SessionId(crate::ids::new_uuid());
        let mut prepared =
            self.manager
                .prepare_fresh_program_runtime(session_id, None, None, (rows, cols))?;
        prepared.wait_until_ready().await?;
        self.manager
            .commit_fresh_runtime(session_id, &mut prepared, || Ok(()))?;
        Ok(Pane(session_id))
    }

    fn pane(&self, pane: Pane) -> Option<&PaneRuntimeHandle> {
        self.manager.runtimes.get(&pane.0)?.panes.values().next()
    }

    /// The panes open.
    #[must_use]
    pub fn panes(&self) -> Vec<Pane> {
        self.manager.runtimes.keys().copied().map(Pane).collect()
    }

    /// Type `bytes` into `pane`. Returns whether the pane took them.
    #[must_use]
    pub fn input(&self, pane: Pane, bytes: &[u8]) -> bool {
        self.pane(pane).is_some_and(|handle| {
            handle
                .input_tx
                .send(PaneRuntimeCommand::Input(bytes.to_vec()))
                .is_ok()
        })
    }

    /// Resize `pane`'s terminal. Returns whether the pane exists.
    #[must_use]
    pub fn resize(&self, pane: Pane, rows: u16, cols: u16) -> bool {
        self.pane(pane).is_some_and(|handle| {
            handle.resize_pty(rows, cols);
            true
        })
    }

    /// The size last requested for `pane`.
    #[must_use]
    pub fn requested_size(&self, pane: Pane) -> Option<(u16, u16)> {
        self.pane(pane)
            .and_then(|handle| handle.last_requested_size.lock().ok().map(|size| *size))
    }

    /// The id of `pane`'s program, while known.
    #[must_use]
    pub fn pid(&self, pane: Pane) -> Option<u32> {
        self.pane(pane)
            .and_then(|handle| handle.process_id.lock().ok().and_then(|pid| *pid))
    }

    /// Whether `pane`'s program exited.
    #[must_use]
    pub fn exited(&self, pane: Pane) -> bool {
        self.pane(pane)
            .is_some_and(|handle| handle.exited.load(Ordering::SeqCst))
    }

    /// Bytes of output `pane` has taken in.
    #[must_use]
    pub fn output_bytes(&self, pane: Pane) -> u64 {
        self.pane(pane).map_or(0, |handle| {
            handle.execution.output_watermark.load(Ordering::SeqCst)
        })
    }

    /// Close `pane` and wait for its runtime to shut down.
    ///
    /// # Errors
    ///
    /// * If there is no such pane
    pub async fn close(&mut self, pane: Pane) -> anyhow::Result<Closed> {
        let execution = Arc::clone(
            &self
                .pane(pane)
                .ok_or_else(|| anyhow::anyhow!("no pane {:?}", pane.0))?
                .execution,
        );
        let removed = self.manager.remove_runtime(pane.0)?;
        shutdown_runtime_handle(&self.host, removed).await;
        Ok(Closed {
            drained: execution.output_drained.load(Ordering::SeqCst),
            cleanup_error: execution.cleanup_error(),
        })
    }

    /// Restart `pane`: its program is shut down in the background and a fresh shell starts in
    /// its place. Waits (in simulated time, at most ten seconds) for the new program to start.
    ///
    /// # Errors
    ///
    /// * If there is no such pane, or the new program never starts
    pub async fn restart(&mut self, pane: Pane) -> anyhow::Result<()> {
        // The new program may reuse the old one's id: tell them apart by pane.
        let restarted = self.manager.restart_pane(pane.0, None)?;
        let deadline = switchy_time::Instant::now() + std::time::Duration::from_secs(10);
        while !self.pane(pane).is_some_and(|handle| {
            handle.meta.id == restarted && handle.process_id.lock().is_ok_and(|pid| pid.is_some())
        }) {
            anyhow::ensure!(
                switchy_time::Instant::now() < deadline,
                "the restarted pane's program never started"
            );
            switchy_async::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(())
    }

    /// Die: from now on the runtime stops, inspects and cleans up none of its sessions (the
    /// operating system still closes its handles), then everything in it is torn down, leaving
    /// the sessions' processes and records as a crash would.
    pub async fn crash(&mut self) {
        self.host.sessions().abandon();
        // The operating system closes the dead server's terminals.
        let terminals: Vec<String> = self
            .panes()
            .into_iter()
            .filter_map(|pane| self.pid(pane))
            .filter_map(|pid| switchy_pty::simulator::process_io(pid)?.terminal)
            .collect();
        switchy_pty::simulator::close_masters(&terminals);
        for pane in self.panes() {
            let _ = self.close(pane).await;
        }
    }

    /// Exit notices the runtime sent since last asked.
    pub fn take_exits(&mut self) -> usize {
        let mut count = 0;
        while self.exits.try_recv().is_ok() {
            count += 1;
        }
        count
    }
}
