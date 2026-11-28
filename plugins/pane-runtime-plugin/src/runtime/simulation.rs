//! Driving the pane runtime from a deterministic simulation (the `simulator` feature).
//!
//! [`Panes`] is a pane runtime with no server around it: it opens panes running the configured
//! shell on the simulated kernel, types into them, resizes and closes them, and reports what a
//! simulation checks (each pane's process, whether it exited, whether closing drained its
//! output and cleaned up). Everything underneath is the real pane runtime: its tasks, threads,
//! terminals and process control, all on the simulation's executor, threads, clock and kernel.
//!
//! Like the server, it keeps the runtime behind a lock taken only for synchronous steps, so
//! several drivers can share one [`Panes`] and their operations overlap as a server's
//! requests do: a pane may be closed while another opens, restarted while it is typed into,
//! or crashed while it starts.

use bmux_attach_layout_protocol::{PaneSelector, PaneSplitDirection};
use bmux_session_models::{ClientId, SessionId};
use uuid::Uuid;
use switchy_thread::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc;

use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
    TerminalExecutionRef as ExecutionRef, TerminalProgramOptions,
};

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

/// A pane runtime under simulation: a server of its own, whose work runs on its own executor,
/// as a server process's does. The run's executor drives it (a pump task runs one of its tasks
/// per turn), so it shares the run's clock.
pub struct Panes {
    server: Arc<Server>,
    /// The server's executor. Every operation runs there, so everything the pane runtime starts
    /// (tasks, and the threads they start) belongs to the server and dies with it.
    runtime: switchy_async::runtime::Runtime,
    /// Ends the task that drives the executor.
    stop: Arc<AtomicBool>,
    /// Each observing client's replica of a pane, built from the frames it was sent (see
    /// [`Panes::observe`]).
    observers: Mutex<std::collections::BTreeMap<(Pane, u64), Replica>>,
}

/// A client's replica of a pane's grid, and where its frames left off.
struct Replica {
    grid: bmux_terminal_grid::TerminalGrid,
    cursor: bmux_terminal_grid::ReplicaCursor,
}

/// What a server holds.
struct Server {
    manager: Arc<Mutex<SessionRuntimeManager>>,
    host: PaneRuntimeHost,
    exits: Mutex<mpsc::UnboundedReceiver<PaneExitEvent>>,
}

/// One pane: a session's (each session starts with one; splits add more).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pane {
    session: SessionId,
    pane: Uuid,
}

/// How closing a pane went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closed {
    /// Its output was drained before the pane ended.
    pub drained: bool,
    /// Cleaning up its process or terminal failed: the first error, if so.
    pub cleanup_error: Option<String>,
}

/// A program execution the execution service launched (see [`Panes::execute`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Execution(ExecutionRef);

impl Execution {
    /// Its pane.
    #[must_use]
    pub const fn pane(&self) -> Pane {
        Pane {
            session: SessionId(self.0.session_id),
            pane: self.0.pane_id,
        }
    }
}

/// What a launch receipt says (see [`Panes::execute`] and [`Panes::reconcile`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// The execution its reservation names.
    pub execution: Execution,
    /// Its launch's phase (`Reserved`, `Running`, `Exited`, `Retired`, ...).
    pub phase: String,
    /// The program's exit code, once recorded.
    pub exit_code: Option<u32>,
    /// The signal that ended it, once recorded.
    pub exit_signal: Option<String>,
    /// What went wrong, if anything.
    pub error: Option<String>,
}

impl Receipt {
    fn of(receipt: &bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalLaunchReceipt) -> Self {
        Self {
            execution: Execution(receipt.reservation.execution.clone()),
            phase: format!("{:?}", receipt.phase),
            exit_code: receipt.exit_code,
            exit_signal: receipt.exit_signal.clone(),
            error: receipt.error.clone(),
        }
    }
}

/// How a running execution stands (see [`Panes::execution_status`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionStatus {
    /// Its terminal session ended, with its output drained.
    pub ended: bool,
    /// The program's exit code, when it exited (not killed by a signal).
    pub exit_code: Option<u32>,
    /// The signal that ended it.
    pub signal: Option<String>,
    /// What went wrong, if anything.
    pub error: Option<String>,
}

/// How restarting a pane went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restarted {
    /// Its new program runs, with this id.
    Running(u32),
    /// The pane was closed (or the runtime crashed) before its new program started.
    Gone,
    /// The new program failed to start (the pane says so, and stays).
    Failed,
}

impl std::fmt::Debug for Panes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Panes").finish_non_exhaustive()
    }
}

impl Drop for Panes {
    fn drop(&mut self) {
        self.stop();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> switchy_thread::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(switchy_thread::sync::PoisonError::into_inner)
}

impl Server {
    fn with_pane<R>(&self, pane: Pane, f: impl FnOnce(&PaneRuntimeHandle) -> R) -> Option<R> {
        let manager = lock(&self.manager);
        manager
            .runtimes
            .get(&pane.session)
            .and_then(|runtime| runtime.panes.get(&pane.pane))
            .map(f)
    }

    fn panes(&self) -> Vec<Pane> {
        lock(&self.manager)
            .runtimes
            .iter()
            .flat_map(|(session, runtime)| {
                runtime.panes.keys().map(|pane| Pane {
                    session: *session,
                    pane: *pane,
                })
            })
            .collect()
    }

    /// Wait until `pane`'s program (`program`, its pane's id) runs, failed to start, or the pane
    /// went (or now runs another program).
    async fn started(&self, pane: Pane, program: Uuid) -> Restarted {
        loop {
            let state = self.with_pane(pane, |handle| {
                (handle.meta.id == program).then(|| {
                    (
                        handle.process_id.lock().ok().and_then(|pid| *pid),
                        handle.exited.load(Ordering::SeqCst),
                    )
                })
            });
            match state {
                None | Some(None) => return Restarted::Gone,
                Some(Some((Some(pid), _))) => return Restarted::Running(pid),
                Some(Some((None, true))) => return Restarted::Failed,
                Some(Some((None, false))) => {}
            }
            switchy_async::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    async fn open(&self, rows: u16, cols: u16) -> anyhow::Result<Pane> {
        let session_id = SessionId(crate::ids::new_uuid());
        let prepared = lock(&self.manager).prepare_fresh_program_runtime(
            session_id,
            None,
            None,
            (rows, cols),
        );
        let mut prepared = prepared?;
        prepared.wait_until_ready().await?;
        let mut manager = lock(&self.manager);
        manager.commit_fresh_runtime(session_id, &mut prepared, || Ok(()))?;
        let pane = manager
            .runtimes
            .get(&session_id)
            .and_then(|runtime| runtime.panes.keys().next().copied())
            .ok_or_else(|| anyhow::anyhow!("the opened session has no pane"))?;
        Ok(Pane {
            session: session_id,
            pane,
        })
    }

    async fn split(&self, pane: Pane, vertical: bool) -> anyhow::Result<(Pane, Restarted)> {
        let direction = if vertical {
            PaneSplitDirection::Vertical
        } else {
            PaneSplitDirection::Horizontal
        };
        let created = lock(&self.manager).create_split_pane(
            pane.session,
            Some(PaneSelector::ById(pane.pane)),
            direction,
            None,
            None,
            None,
        )?;
        let new = Pane {
            session: pane.session,
            pane: created,
        };
        Ok((new, self.started(new, created).await))
    }

    async fn close(&self, pane: Pane) -> anyhow::Result<Closed> {
        let (execution, removed) = {
            let mut manager = lock(&self.manager);
            let runtime = manager
                .runtimes
                .get(&pane.session)
                .ok_or_else(|| anyhow::anyhow!("no session {:?}", pane.session))?;
            let execution = Arc::clone(
                &runtime
                    .panes
                    .get(&pane.pane)
                    .ok_or_else(|| anyhow::anyhow!("no pane {:?}", pane.pane))?
                    .execution,
            );
            // Its session's last pane closes with the session; another closes alone, its
            // shutdown in the background (as a client's close-pane request does).
            if runtime.panes.len() > 1 {
                manager.close_pane(pane.session, Some(PaneSelector::ById(pane.pane)))?;
                (execution, None)
            } else {
                (execution, Some(manager.remove_runtime(pane.session)?))
            }
        };
        let Some(removed) = removed else {
            return Ok(Closed {
                drained: false,
                cleanup_error: None,
            });
        };
        shutdown_runtime_handle(&self.host, removed).await;
        Ok(Closed {
            drained: execution.output_drained.load(Ordering::SeqCst),
            cleanup_error: execution.cleanup_error(),
        })
    }

    async fn restart(&self, pane: Pane) -> anyhow::Result<Restarted> {
        let restarted = lock(&self.manager)
            .restart_pane(pane.session, Some(PaneSelector::ById(pane.pane)))?;
        Ok(self.started(pane, restarted).await)
    }
}

impl Panes {
    /// A pane runtime whose panes run `shell` (a program registered with the simulated
    /// kernel, by its base name).
    #[must_use]
    pub fn new(shell: &str) -> Self {
        Self::build(shell, None, false)
    }

    /// As [`Self::new`], recording each pane's session under `records` (as an activation
    /// with a runtime directory does), after sweeping the sessions a crashed predecessor
    /// recorded there.
    #[must_use]
    pub fn recovering(shell: &str, records: &std::path::Path) -> Self {
        Self::build(shell, Some(records), false)
    }

    /// As [`Self::recovering`], activated as a managed companion of one execution target (as a
    /// launch with `BMUX_MANAGED_TARGET_ID` and `BMUX_MANAGED_TARGET_IDENTITY` is): its
    /// executions' store is that target's, which one writer holds at a time.
    #[must_use]
    pub fn recovering_managed(shell: &str, records: &std::path::Path) -> Self {
        Self::build(shell, Some(records), true)
    }

    fn build(shell: &str, records: Option<&std::path::Path>, managed: bool) -> Self {
        let (exit_tx, exits) = mpsc::unbounded_channel();
        let host = PaneRuntimeHost::detached();
        if managed {
            super::managed_scope::select(&host, MANAGED_IDENTITY, MANAGED_TARGET)
                .expect("the simulation's managed target is valid");
        }
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
        let runtime = switchy_async::runtime::Runtime::new();
        let manager = Arc::new(Mutex::new(manager));
        register_execution_service(&host, &manager, &runtime, records);
        let stop = Arc::new(AtomicBool::new(false));
        let (pumped, stopped) = (runtime.clone(), Arc::clone(&stop));
        // On the current (the run's) executor, not the server's: it is the server's processor.
        drop(switchy_async::task::spawn(async move {
            while !stopped.load(Ordering::SeqCst) && !pumped.is_shut_down() {
                pumped.tick();
                switchy_async::task::yield_now().await;
            }
        }));
        Self {
            server: Arc::new(Server {
                manager,
                host,
                exits: Mutex::new(exits),
            }),
            runtime,
            stop,
            observers: Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Whether panes' programs' leftover jobs are swept when the program exits (the
    /// `execution.sweep_on_exit` setting; on by default).
    pub fn set_sweep_on_exit(&self, sweep: bool) {
        self.server.host.sessions().set_sweep_on_exit(sweep);
    }

    /// Split `pane` (side by side, or one above the other): a new pane in its session runs a
    /// new program. Waits until that program runs, fails to start, or the pane goes.
    ///
    /// # Errors
    ///
    /// * If the split is refused, or the server died first
    pub async fn split(&self, pane: Pane, vertical: bool) -> anyhow::Result<(Pane, Restarted)> {
        self.on_server(move |server| Box::pin(async move { server.split(pane, vertical).await }))
            .await
            .ok_or_else(|| anyhow::anyhow!("the server died"))?
    }

    /// The serials of the session leaders whose left-running jobs the runtime released (with
    /// `sweep_on_exit` off, once the pane's terminal reached end of file): they keep running,
    /// whatever happens to the pane, as `docs/pane-execution.md` says.
    #[must_use]
    pub fn released_sessions(&self) -> Vec<u64> {
        released().lock().map(|released| released.clone()).unwrap_or_default()
    }

    /// The panes `pane`'s session has (with it).
    #[must_use]
    pub fn siblings(&self, pane: Pane) -> Vec<Pane> {
        self.panes()
            .into_iter()
            .filter(|other| other.session == pane.session)
            .collect()
    }

    /// Stop the server's executor, once its panes are closed: as a server exits, it stops
    /// running anything.
    pub fn shut_down(&self) {
        self.stop();
    }

    /// End the task that drives the server's executor.
    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Run `work` on the server's executor; `None` if the server died first.
    async fn on_server<T: Send + Unpin + 'static>(
        &self,
        work: impl FnOnce(Arc<Server>) -> std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>,
    ) -> Option<T> {
        self.runtime
            .handle()
            .spawn(work(Arc::clone(&self.server)))
            .await
            .ok()
    }

    /// Open a pane of `rows` by `cols`, once its program is ready.
    ///
    /// # Errors
    ///
    /// * If the pane cannot be prepared, started or committed, or the server died first
    pub async fn open(&self, rows: u16, cols: u16) -> anyhow::Result<Pane> {
        self.on_server(move |server| Box::pin(async move { server.open(rows, cols).await }))
            .await
            .ok_or_else(|| anyhow::anyhow!("the server died"))?
    }

    /// The panes open.
    #[must_use]
    pub fn panes(&self) -> Vec<Pane> {
        self.server.panes()
    }

    /// Type `bytes` into `pane`. Returns whether the pane took them.
    #[must_use]
    pub fn input(&self, pane: Pane, bytes: &[u8]) -> bool {
        self.server
            .with_pane(pane, |handle| {
                handle
                    .input_tx
                    .send(PaneRuntimeCommand::Input(bytes.to_vec()))
                    .is_ok()
            })
            .unwrap_or(false)
    }

    /// Resize `pane`'s terminal. Returns whether the pane exists.
    #[must_use]
    pub fn resize(&self, pane: Pane, rows: u16, cols: u16) -> bool {
        self.server
            .with_pane(pane, |handle| handle.resize_pty(rows, cols))
            .is_some()
    }

    /// The size last requested for `pane`.
    #[must_use]
    pub fn requested_size(&self, pane: Pane) -> Option<(u16, u16)> {
        self.server
            .with_pane(pane, |handle| {
                handle.last_requested_size.lock().ok().map(|size| *size)
            })
            .flatten()
    }

    /// The id of `pane`'s program, while known.
    #[must_use]
    pub fn pid(&self, pane: Pane) -> Option<u32> {
        self.server
            .with_pane(pane, |handle| {
                handle.process_id.lock().ok().and_then(|pid| *pid)
            })
            .flatten()
    }

    /// The serial (in the simulated kernel) of `pane`'s program, once its session is adopted:
    /// unlike its id, never another process's.
    #[must_use]
    pub fn program_serial(&self, pane: Pane) -> Option<u64> {
        self.server
            .with_pane(pane, |handle| {
                handle
                    .execution
                    .session
                    .lock()
                    .ok()
                    .and_then(|session| session.as_ref().and_then(|control| control.leader_serial()))
            })
            .flatten()
    }

    /// Whether `pane`'s program exited.
    #[must_use]
    pub fn exited(&self, pane: Pane) -> bool {
        self.server
            .with_pane(pane, |handle| handle.exited.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// Whether `pane`'s grid shows what a replay of everything it took in shows (the grid's
    /// record of its output and resizes): `None` when there is no such pane.
    #[must_use]
    pub fn screen_check(&self, pane: Pane) -> Option<Result<(), String>> {
        self.server.with_pane(pane, |handle| {
            handle
                .terminal_grid
                .lock()
                .map_err(|_| "the grid lock was poisoned".to_owned())
                .and_then(|grid| grid.check_record())
        })
    }

    /// The chunks of output `pane`'s grid took in, in order: its programs' output as read, and
    /// the runtime's own notices (which start `\r\n[bmux] `). `None` when there is no such pane.
    #[must_use]
    pub fn taken_in(&self, pane: Pane) -> Option<Vec<Vec<u8>>> {
        self.server
            .with_pane(pane, |handle| {
                handle.terminal_grid.lock().ok().map(|grid| {
                    grid.recorded_output()
                        .into_iter()
                        .map(<[u8]>::to_vec)
                        .collect()
                })
            })
            .flatten()
    }

    /// As client `client` observing `pane` without attaching does: take the frames the pane has
    /// for it and apply them to its replica of the pane's grid (a new replica's first frame
    /// is a reset). Returns whether the pane exists.
    ///
    /// # Errors
    ///
    /// * A frame that does not apply to the replica
    pub fn observe(&self, pane: Pane, client: u64) -> Result<bool, String> {
        let refs = lock(&self.server.manager)
            .pane_data_refs_for_targets(&[(pane.session, pane.pane)]);
        if refs.is_empty() {
            return Ok(false);
        }
        let client_id = ClientId(Uuid::from_u128(u128::from(client)));
        let mut observers = lock(&self.observers);
        let replica = observers.entry((pane, client)).or_insert_with(|| Replica {
            grid: bmux_terminal_grid::TerminalGrid::new(
                1,
                1,
                bmux_terminal_grid::GridLimits::default(),
            )
            .expect("a 1x1 grid"),
            cursor: bmux_terminal_grid::ReplicaCursor::default(),
        });
        loop {
            let (frames, pending) = super::frames_for_panes(&refs, client_id, usize::MAX)
                .map_err(|error| format!("taking frames failed: {error:?}"))?;
            for frame in frames {
                replica
                    .grid
                    .apply_frame(&frame.encoded, &mut replica.cursor)
                    .map_err(|error| format!("a frame did not apply: {error:?}"))?;
            }
            if !pending {
                return Ok(true);
            }
        }
    }

    /// Client `client` stops observing `pane` (as a detaching client): the server forgets its
    /// replica, so observing again starts over.
    pub fn unobserve(&self, pane: Pane, client: u64) {
        lock(&self.observers).remove(&(pane, client));
        let refs = lock(&self.server.manager)
            .pane_data_refs_for_targets(&[(pane.session, pane.pane)]);
        let client_id = ClientId(Uuid::from_u128(u128::from(client)));
        for pane in refs {
            if let Ok(mut replicas) = pane.frame_replicas.lock() {
                replicas.remove(&client_id);
            }
        }
    }

    /// Whether client `client`'s replica of `pane`, brought up to date, shows what the pane's
    /// grid shows; `None` when it does not observe the pane. It stops observing.
    ///
    /// The pane may still take in output, so the grid is read (its revision and screen), then
    /// the replica brought up to date: when the revision held still meanwhile, the replica
    /// must show that screen.
    #[must_use]
    pub fn observed_level(&self, pane: Pane, client: u64) -> Option<Result<(), String>> {
        if !lock(&self.observers).contains_key(&(pane, client)) {
            return None;
        }
        let checked = self.level_once(pane, client);
        self.unobserve(pane, client);
        Some(checked)
    }

    fn level_once(&self, pane: Pane, client: u64) -> Result<(), String> {
        let text = |rows: &[bmux_terminal_grid::RowSnapshot]| -> Vec<String> {
            rows.iter()
                .map(|row| {
                    row.runs
                        .iter()
                        .map(|run| run.text.as_str())
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect()
        };
        let grid_now = || {
            self.server
                .with_pane(pane, |handle| {
                    handle
                        .terminal_grid
                        .lock()
                        .map(|grid| {
                            let grid = grid.grid();
                            (grid.revision(), grid.height(), grid.snapshot(0, grid.height()).rows)
                        })
                        .map_err(|_| "the grid lock was poisoned".to_owned())
                })
                .ok_or_else(|| "the pane is gone".to_owned())?
        };
        let mut last = None;
        for _ in 0..16 {
            let (revision, height, rows) = grid_now()?;
            self.observe(pane, client)?;
            if grid_now()?.0 != revision {
                continue;
            }
            let observers = lock(&self.observers);
            let replica = &observers
                .get(&(pane, client))
                .ok_or("the replica is gone")?
                .grid;
            let shown = replica.snapshot(0, replica.height()).rows;
            if (replica.height(), &shown) == (height, &rows) {
                return Ok(());
            }
            last = Some(format!(
                "the replica shows {:?} ({} rows), the grid {:?} ({height} rows)",
                text(&shown),
                replica.height(),
                text(&rows),
            ));
            break;
        }
        Err(last.unwrap_or_else(|| "the grid never held still".to_owned()))
    }

    /// Client `client` attaches to `pane`'s session (once) and takes an attach snapshot of
    /// `pane`'s grid, as an attaching client does (and again on a layout change), hydrating a
    /// grid from it as `bmux attach` does. Returns whether the pane was there.
    ///
    /// # Errors
    ///
    /// * The snapshot shows a state the pane's grid never passed through, by the output offset
    ///   it names, or attaching failed otherwise
    pub fn attach(&self, pane: Pane, client: u64) -> Result<bool, String> {
        let client_id = attaching_client(client);
        let refs = {
            let mut manager = lock(&self.server.manager);
            let Some(runtime) = manager.runtimes.get(&pane.session) else {
                return Ok(false);
            };
            if !runtime.panes.contains_key(&pane.pane) {
                return Ok(false);
            }
            if !runtime.attached_clients.contains(&client_id) {
                manager
                    .begin_attach(pane.session, client_id)
                    .map_err(|error| format!("attaching failed: {error:?}"))?;
            }
            manager
                .attach_pane_data_refs_for_session_panes(pane.session, client_id, &[pane.pane])
                .map_err(|error| format!("resolving the attached pane failed: {error:?}"))?
        };
        let rows = usize::from(u16::MAX);
        let state = match super::grid_snapshots_for_panes(refs, client_id, rows) {
            Ok(state) => state,
            // Over the reply budget, a client gets no snapshot: it shows live state instead.
            Err(super::SessionRuntimeError::ResponseBudgetExceeded) => return Ok(true),
            Err(error) => return Err(format!("taking an attach snapshot failed: {error:?}")),
        };
        for captured in state.snapshots {
            let snapshot: bmux_terminal_grid::GridSnapshot =
                serde_json::from_slice(&captured.encoded)
                    .map_err(|error| format!("an attach snapshot did not decode: {error}"))?;
            bmux_terminal_grid::TerminalGridStream::from_snapshot(
                &snapshot,
                bmux_terminal_grid::GridLimits::default(),
            )
            .map_err(|error| format!("an attach snapshot did not hydrate: {error}"))?;
            let checked = self.server.with_pane(pane, |handle| {
                handle
                    .terminal_grid
                    .lock()
                    .map_err(|_| "the grid lock was poisoned".to_owned())?
                    .check_passed_through(&snapshot, rows, captured.stream_end)
            });
            checked.unwrap_or(Ok(()))?;
        }
        Ok(true)
    }

    /// Client `client` detaches from `pane`'s session (as [`Self::attach`] attached it).
    pub fn detach(&self, pane: Pane, client: u64) {
        lock(&self.server.manager).end_attach(pane.session, attaching_client(client));
    }

    /// Run `work` on a thread of the server's that no executor runs on, as a service request
    /// is (the execution service drives the server's executor from it).
    ///
    /// # Errors
    ///
    /// * The server crashed meanwhile (its threads die with it)
    async fn on_request_thread<R: Send + Unpin + 'static>(
        &self,
        work: impl FnOnce() -> Result<R, String> + Send + 'static,
    ) -> Result<R, String> {
        // A thread started from one of the server's belongs to the server.
        self.runtime
            .spawn_blocking(move || {
                // Starting a thread may fail (the thread-spawn fault), as a service's can.
                switchy_thread::thread::Builder::new()
                    .name("request".into())
                    .spawn(work)
                    .map_err(|error| format!("starting a request thread failed: {error}"))?
                    .join()
                    .map_err(|_| "the server went".to_owned())?
            })
            .await
            .map_err(|error| format!("the server went: {error}"))?
    }

    /// Reserve running `program` with `args` under the mutation id `mutation`, then launch
    /// that reservation, as a client of the execution service does (a client retrying with the
    /// same id gets the same reservation, and launching it again starts nothing new). The work
    /// runs on one of the server's threads, as a service request's does.
    ///
    /// # Errors
    ///
    /// * What the execution service refused, or the server crashed meanwhile
    pub async fn execute(
        &self,
        mutation: u128,
        program: &str,
        args: Vec<String>,
        (rows, cols): (u16, u16),
    ) -> Result<Receipt, String> {
        let host = self.server.host.clone();
        let options = TerminalProgramOptions {
            program: program.to_owned(),
            args,
            cwd: None,
            env: Default::default(),
            cols,
            rows,
        };
        self.on_request_thread(move || {
                let reservation = super::reservations::reserve(
                    &host,
                    Uuid::from_u128(mutation),
                    options,
                    |_| Ok(()),
                )
                .map_err(|error| format!("reserving failed: {error:?}"))?;
                super::reservations::launch(&host, &reservation, |_| Ok(()))
                    .map(|receipt| Receipt::of(&receipt))
                    .map_err(|error| format!("launching failed: {error:?}"))
            })
        .await
    }

    /// How `execution` stands now; `None` once the runtime no longer knows it.
    #[must_use]
    pub fn execution_status(&self, execution: &Execution) -> Option<ExecutionStatus> {
        super::execution::status(&self.server.host, &execution.0)
            .ok()
            .map(|status| ExecutionStatus {
                ended: status.terminal_session_ended && status.output_drained,
                exit_code: status.exit_code,
                signal: status.signal,
                error: status.error,
            })
    }

    /// Reconcile `execution`'s launch receipt with the runtime, as a client does after an
    /// outage (it records the exit of one that ended), returning what it says.
    ///
    /// # Errors
    ///
    /// * What the execution service refused, or the server crashed meanwhile
    pub async fn reconcile(&self, execution: &Execution) -> Result<Receipt, String> {
        let host = self.server.host.clone();
        let reference = execution.0.clone();
        self.on_request_thread(move || {
                super::reservations::reconcile(&host, &reference)
                    .map(|reconciled| Receipt::of(&reconciled.launch))
                    .map_err(|error| format!("reconciling failed: {error:?}"))
            })
        .await
    }

    /// Retire `execution`, which ended (reconciled to its last revision), as a client done
    /// with it does.
    ///
    /// # Errors
    ///
    /// * What the execution service refused, or the server crashed meanwhile
    pub async fn retire(&self, execution: &Execution) -> Result<(), String> {
        let host = self.server.host.clone();
        let reference = execution.0.clone();
        self.on_request_thread(move || {
                let reconciled = super::reservations::reconcile(&host, &reference)
                    .map_err(|error| format!("reconciling failed: {error:?}"))?;
                super::reservations::retire(
                    &host,
                    &reference,
                    crate::ids::new_uuid(),
                    reconciled.launch.revision,
                )
                .map(|_| ())
                .map_err(|error| format!("retiring failed: {error:?}"))
            })
        .await
    }

    /// Bytes of output `pane` has taken in.
    #[must_use]
    pub fn output_bytes(&self, pane: Pane) -> u64 {
        self.server
            .with_pane(pane, |handle| {
                handle.execution.output_watermark.load(Ordering::SeqCst)
            })
            .unwrap_or(0)
    }

    /// Close `pane` and wait for its runtime to shut down.
    ///
    /// # Errors
    ///
    /// * If there is no such pane, or the server died first
    pub async fn close(&self, pane: Pane) -> anyhow::Result<Closed> {
        self.on_server(move |server| Box::pin(async move { server.close(pane).await }))
            .await
            .ok_or_else(|| anyhow::anyhow!("the server died"))?
    }

    /// Restart `pane`: its program is shut down in the background and a fresh shell starts in
    /// its place. Waits until the new program starts, fails to, or the pane goes.
    ///
    /// # Errors
    ///
    /// * If there is no such pane
    pub async fn restart(&self, pane: Pane) -> anyhow::Result<Restarted> {
        self.on_server(move |server| Box::pin(async move { server.restart(pane).await }))
            .await
            .unwrap_or(Ok(Restarted::Gone))
    }

    /// Die, as a killed server process does: from now on its sessions are neither stopped,
    /// inspected nor cleaned up, and then every task and thread it has stops where it is. What
    /// they held is dropped as the operating system closes a dead process's handles (a
    /// terminal's master side hangs up; a Windows job ends its processes), and everything else
    /// (processes, session records) is left as a crash leaves it.
    pub fn crash(&self) {
        self.server.host.sessions().abandon();
        self.runtime.shutdown_now();
        self.stop();
        // What the dead server's manager still holds closes too.
        let runtimes = lock(&self.server.manager).remove_all_runtimes();
        drop(runtimes);
    }

    /// The panes whose exits the runtime acted on since last asked, one per notice (as its
    /// exit processing does, it drops notices about a program a restart replaced).
    pub fn take_exits(&self) -> Vec<Pane> {
        let mut exits = lock(&self.server.exits);
        let mut panes = Vec::new();
        while let Ok(event) = exits.try_recv() {
            if event.is_current(&lock(&self.server.manager)) {
                panes.push(Pane {
                    session: event.session_id,
                    pane: event.pane_id,
                });
            }
        }
        panes
    }
}

/// Register what the execution service finds through the host, as an activation does: the
/// runtime its work runs on, the session manager its sessions join, and (with a runtime
/// directory) its reservation store, in `executions` beside `records`, retiring what a
/// crashed predecessor left launched.
fn register_execution_service(
    host: &PaneRuntimeHost,
    manager: &Arc<Mutex<SessionRuntimeManager>>,
    runtime: &switchy_async::runtime::Runtime,
    records: Option<&std::path::Path>,
) {
    let pane_inputs = Arc::clone(&lock(manager).pane_input_index);
    host.states()
        // simulation-safe: core's state registry type, written only at registration
        .register(&Arc::new(std::sync::RwLock::new(super::SpawnRuntimeHandle(
            Arc::clone(manager),
            runtime.handle(),
            pane_inputs,
        ))));
    host.states()
        // simulation-safe: core's state registry type, written only at registration
        .register(&Arc::new(std::sync::RwLock::new(
            bmux_session_state::SessionManagerHandle::new(Sessions::default()),
        )));
    if let Some(domain) = records.and_then(std::path::Path::parent) {
        let domain = domain.join("executions");
        // A server's state directory, made before its first activation, survives a power cut.
        if crate::atomic_state::create_dir_all_durable(&domain).is_ok() {
            let mut durable = Some(domain.as_path());
            while let Some(directory) = durable {
                let _ = switchy_fs::sync::sync_directory(directory);
                durable = directory.parent();
            }
            super::reservations::activate_at(host, &domain);
        }
    }
}

/// The sessions programs' executions join (the sessions plugin's part).
#[derive(Default)]
struct Sessions(Mutex<std::collections::BTreeMap<SessionId, bmux_session_models::Session>>);

impl bmux_session_state::SessionManagerReader for Sessions {
    fn list_sessions(&self) -> Vec<bmux_session_models::SessionInfo> {
        Vec::new()
    }
    fn get_session(&self, session_id: SessionId) -> Option<bmux_session_models::Session> {
        lock(&self.0).get(&session_id).cloned()
    }
    fn contains(&self, session_id: SessionId) -> bool {
        lock(&self.0).contains_key(&session_id)
    }
}

impl bmux_session_state::SessionManagerWriter for Sessions {
    fn create_session(&self, name: Option<String>) -> anyhow::Result<SessionId> {
        let session = bmux_session_models::Session::with_id(
            SessionId(crate::ids::new_uuid()),
            name,
        );
        let id = session.id;
        lock(&self.0).insert(id, session);
        Ok(id)
    }
    fn insert_session(&self, session: bmux_session_models::Session) -> anyhow::Result<()> {
        lock(&self.0).insert(session.id, session);
        Ok(())
    }
    fn rename_session(&self, _session_id: SessionId, _name: String) -> anyhow::Result<()> {
        Ok(())
    }
    fn remove_session(&self, session_id: SessionId) -> anyhow::Result<()> {
        lock(&self.0).remove(&session_id);
        Ok(())
    }
    fn add_client(&self, _session_id: SessionId, _client_id: ClientId) {}
    fn remove_client(&self, _session_id: SessionId, _client_id: &ClientId) {}
    fn snapshot(&self) -> bmux_session_state::SessionManagerSnapshot {
        bmux_session_state::SessionManagerSnapshot::default()
    }
    fn restore_snapshot(&self, _snapshot: bmux_session_state::SessionManagerSnapshot) {}
}

/// The managed execution target [`Panes::recovering_managed`] serves, and its identity.
const MANAGED_TARGET: &str = "5d1e6a7e5d1e6a7e5d1e6a7e5d1e6a7e5d1e6a7e5d1e6a7e5d1e6a7e5d1e6a7e";
const MANAGED_IDENTITY: &str = r#"{"simulation":"pane-runtime"}"#;

/// The client id [`Panes::attach`] attaches client `client` as (apart from observers').
fn attaching_client(client: u64) -> ClientId {
    ClientId(Uuid::from_u128((1 << 64) | u128::from(client)))
}

/// The run's record of released sessions (see [`Panes::released_sessions`]).
struct ReleasedSessions;

fn released() -> Arc<Mutex<Vec<u64>>> {
    crate::run_shared::run_shared::<ReleasedSessions, _>(|| Mutex::new(Vec::new()))
}

/// Record that the runtime released the jobs of the session `leader` (a serial) led.
pub(crate) fn record_release(leader: u64) {
    if let Ok(mut released) = released().lock() {
        released.push(leader);
    }
}
