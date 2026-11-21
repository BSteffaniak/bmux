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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed {
    /// Its output was drained before the pane ended.
    pub drained: bool,
    /// Cleaning up its process or terminal failed.
    pub cleanup_failed: bool,
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
            cleanup_failed: execution.cleanup_failed.load(Ordering::SeqCst),
        })
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
