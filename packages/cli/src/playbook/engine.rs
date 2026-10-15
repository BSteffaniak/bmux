//! Application adapters for the single reusable playbook executor.
use super::RunOptions;
use crate::pane_runtime_client::BmuxPaneRuntimeClientExt;
use crate::runtime::attach::runtime::{
    AttachTerminal, AttachTerminalEventFuture, HeadlessAttachTerminal, HeadlessAttachTerminalHandle,
};
use anyhow::{Context, Result, ensure};
use bmux_attach_token_state::AttachGrant;
use bmux_client::{
    AttachLayoutState, AttachOpenInfo, AttachSnapshotState, BmuxClient, PaneOutputBatchResult,
};
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_context_state::ContextSelector;
use bmux_keybind::RuntimeAction;
use bmux_keyboard::KeyStroke;
use bmux_playbook::host::{
    AttachSimulation, HostAdapter, HostFuture, KeyProcessor, RealAttach, SandboxManifestUpdate,
    VisualTerminal,
};
use bmux_playbook::transport::{ClientFuture, PaneGridSnapshotResult, PaneTransport};
use bmux_session_models::SessionSelector;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::Event as CrosstermEvent;
use crossterm::style::Print;
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use crossterm::{execute, queue};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, warn};
use uuid::Uuid;

use bmux_playbook::engine::crossterm_event_from_stroke;
pub use bmux_playbook::engine::{
    AttachInputRuntime, PlaybookAttachRenderFrameFacts, VisualInteractiveState,
    drain_output_until_idle, drain_output_with_threshold, execute_step, recording_plugin_error,
    start_recording, summarize_attach_render_trace,
};
#[cfg(test)]
use bmux_playbook::engine::{reconcile_real_attach_target, typed_new_session_playbook};

#[derive(Debug)]
pub struct CliHost;
#[must_use]
pub fn host() -> Arc<dyn HostAdapter> {
    Arc::new(CliHost)
}

/// Execute with CLI terminal, signal and process-selection adapters.
/// # Errors
/// Returns execution, transport, cleanup or signal interruption errors.
pub async fn run_playbook(
    playbook: bmux_playbook::types::Playbook,
    target_server: bool,
    options: RunOptions,
) -> Result<bmux_playbook::types::PlaybookResult> {
    let managed = std::env::var_os("BMUX_PLAYBOOK_MANAGED_ROOT").is_some();
    let run = bmux_playbook::run(playbook, target_server, options, host());
    if managed {
        return run.await;
    }
    tokio::select! {
        result = run => result,
        _ = tokio::signal::ctrl_c() => anyhow::bail!("playbook interrupted by signal"),
    }
}

impl PaneTransport for CliHost {
    fn attach_grant<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        selector: SessionSelector,
    ) -> ClientFuture<'a, AttachGrant> {
        Box::pin(async move { BmuxPaneRuntimeClientExt::attach_grant(client, selector).await })
    }
    fn attach_context_grant<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        selector: ContextSelector,
    ) -> ClientFuture<'a, AttachGrant> {
        Box::pin(
            async move { BmuxPaneRuntimeClientExt::attach_context_grant(client, selector).await },
        )
    }
    fn retarget_attach_context<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        context_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> ClientFuture<'a, AttachOpenInfo> {
        Box::pin(async move {
            BmuxPaneRuntimeClientExt::retarget_attach_context(client, context_id, cols, rows).await
        })
    }
    fn open_attach_stream_info<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        grant: &'a AttachGrant,
    ) -> ClientFuture<'a, AttachOpenInfo> {
        Box::pin(
            async move { BmuxPaneRuntimeClientExt::open_attach_stream_info(client, grant).await },
        )
    }
    fn detach<'a>(&'a self, client: &'a mut BmuxClient) -> ClientFuture<'a, ()> {
        Box::pin(async move { BmuxPaneRuntimeClientExt::detach(client).await })
    }
    fn attach_input<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        session_id: Uuid,
        data: Vec<u8>,
    ) -> ClientFuture<'a, usize> {
        Box::pin(
            async move { BmuxPaneRuntimeClientExt::attach_input(client, session_id, data).await },
        )
    }
    fn pane_direct_input<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        session_id: Uuid,
        pane_id: Uuid,
        data: Vec<u8>,
    ) -> ClientFuture<'a, usize> {
        Box::pin(async move {
            BmuxPaneRuntimeClientExt::pane_direct_input(client, session_id, pane_id, data).await
        })
    }
    fn attach_set_viewport<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        session_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> ClientFuture<'a, (u16, u16)> {
        Box::pin(async move {
            BmuxPaneRuntimeClientExt::attach_set_viewport(client, session_id, cols, rows).await
        })
    }
    fn attach_layout<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        session_id: Uuid,
    ) -> ClientFuture<'a, AttachLayoutState> {
        Box::pin(async move { BmuxPaneRuntimeClientExt::attach_layout(client, session_id).await })
    }
    fn attach_pane_output_batch<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        session_id: Uuid,
        pane_ids: Vec<Uuid>,
        max_bytes: usize,
    ) -> ClientFuture<'a, PaneOutputBatchResult> {
        Box::pin(async move {
            BmuxPaneRuntimeClientExt::attach_pane_output_batch(
                client, session_id, pane_ids, max_bytes,
            )
            .await
        })
    }
    fn attach_snapshot<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        session_id: Uuid,
        max_bytes_per_pane: usize,
    ) -> ClientFuture<'a, AttachSnapshotState> {
        Box::pin(async move {
            BmuxPaneRuntimeClientExt::attach_snapshot(client, session_id, max_bytes_per_pane).await
        })
    }
    fn grid_snapshot<'a>(
        &'a self,
        client: &'a mut BmuxClient,
        session: Uuid,
        panes: Vec<Uuid>,
        rows: usize,
    ) -> ClientFuture<'a, Vec<PaneGridSnapshotResult>> {
        Box::pin(async move {
            crate::pane_runtime_client::attach_pane_grid_snapshot_state(
                client, session, panes, rows,
            )
            .await
            .map(|items| {
                items
                    .into_iter()
                    .map(|item| PaneGridSnapshotResult {
                        pane_id: item.pane_id,
                        stream_end: item.stream_end,
                        encoded: item.encoded,
                    })
                    .collect()
            })
        })
    }
}

struct CliKeyProcessor(crate::input::InputProcessor);
impl KeyProcessor for CliKeyProcessor {
    fn set_scroll_mode(&mut self, active: bool) {
        self.0.set_scroll_mode(active);
    }
    fn process_stroke(&mut self, stroke: KeyStroke) -> Vec<RuntimeAction> {
        self.0
            .process_terminal_event(crossterm_event_from_stroke(stroke))
    }
    fn flush(&mut self) -> Vec<RuntimeAction> {
        self.0.process_stream_bytes(&[])
    }
}

impl HostAdapter for CliHost {
    fn executable(&self) -> Result<PathBuf> {
        std::env::current_exe().context("failed resolving bmux executable")
    }
    fn pane_transport(&self) -> Arc<dyn PaneTransport> {
        Arc::new(Self)
    }
    fn parse_key_chord(&self, chord: &str) -> Result<Vec<KeyStroke>> {
        crate::input::parse_key_chord(chord)
    }
    fn key_processor(&self, config: &BmuxConfig) -> Box<dyn KeyProcessor> {
        let modes = config
            .keybindings
            .modes
            .iter()
            .map(|(id, mode)| {
                (
                    id.clone(),
                    crate::input::ModalModeConfig {
                        label: mode.label.clone(),
                        passthrough: mode.passthrough,
                        bindings: mode.bindings.clone(),
                    },
                )
            })
            .collect();
        let keymap = crate::input::Keymap::from_modal_parts_with_scroll(
            config
                .keybindings
                .resolve_timeout()
                .map_or(None, |timeout| timeout.timeout_ms()),
            &config.keybindings.initial_mode,
            &modes,
            &config.keybindings.global,
            &config.keybindings.scroll,
        )
        .unwrap_or_else(|_| crate::input::Keymap::default_runtime());
        Box::new(CliKeyProcessor(crate::input::InputProcessor::new(
            keymap, false,
        )))
    }
    fn terminal_is_interactive(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    }
    fn enter_visual_terminal(&self) -> Result<Box<dyn VisualTerminal>> {
        Ok(Box::new(VisualTerminalGuard::enter()?))
    }
    fn read_line(&self) -> Result<String> {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("failed reading playbook control input")?;
        Ok(line)
    }
    fn start_real_attach(
        &self,
        client: BmuxClient,
        session_id: Uuid,
        config: Option<BmuxConfig>,
        paths: Option<ConfigPaths>,
        viewport: (u16, u16),
    ) -> HostFuture<'_, Box<dyn RealAttach>> {
        Box::pin(async move {
            Ok(Box::new(
                start_cli_attach_runtime(client, session_id, config, paths, viewport).await?,
            ) as Box<dyn RealAttach>)
        })
    }
    fn simulation(&self, cols: u16, rows: u16) -> Result<Box<dyn AttachSimulation>> {
        Ok(Box::new(CliSimulation(
            crate::runtime::attach::sim::AttachSimHarness::new(cols, rows),
        )))
    }
    fn write_sandbox_manifest(&self, update: SandboxManifestUpdate<'_>) -> Result<()> {
        use crate::sandbox_meta::{
            SandboxManifest, SandboxManifestPaths, read_manifest, sandbox_id_from_root,
            unix_millis_now, write_manifest,
        };
        let manifest = SandboxManifest {
            id: sandbox_id_from_root(update.root),
            source: "playbook".to_owned(),
            created_at_unix_ms: read_manifest(update.root)
                .ok()
                .map_or_else(unix_millis_now, |existing| existing.created_at_unix_ms),
            updated_at_unix_ms: unix_millis_now(),
            pid: std::process::id(),
            bmux_bin: update.executable.to_string_lossy().into_owned(),
            command: update.command.to_vec(),
            env_mode: match update.env_mode {
                bmux_playbook::types::SandboxEnvMode::Clean => "clean",
                bmux_playbook::types::SandboxEnvMode::Inherit => "inherit",
            }
            .to_owned(),
            status: update.status.to_owned(),
            exit_code: update.exit_code,
            kept: update.kept,
            paths: SandboxManifestPaths {
                root: update.root.to_string_lossy().into_owned(),
                logs: update.root.join("logs").to_string_lossy().into_owned(),
                runtime: update.paths.runtime_dir.to_string_lossy().into_owned(),
                state: update.paths.state_dir().to_string_lossy().into_owned(),
            },
        };
        write_manifest(update.root, &manifest)
    }
    fn register_sandbox(&self, root: &Path) -> Result<()> {
        crate::sandbox_meta::upsert_index_entry(&crate::sandbox_meta::read_manifest(root)?)
    }
    fn write_sandbox_lock(&self, root: &Path, pid: u32) -> Result<()> {
        crate::sandbox_meta::write_lock(root, pid)
    }
    fn clear_sandbox_lock(&self, root: &Path) -> Result<()> {
        crate::sandbox_meta::clear_lock(root);
        Ok(())
    }
}
#[derive(Debug)]
struct VisualTerminalGuard {
    active: bool,
}

impl VisualTerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed enabling raw mode for visual playbook mode")?;
        if let Err(error) = execute!(std::io::stdout(), EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(anyhow::anyhow!(
                "failed entering alternate screen for visual playbook mode: {error}"
            ));
        }
        Ok(Self { active: true })
    }

    fn suspend_for_line_input(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        execute!(std::io::stdout(), Show, LeaveAlternateScreen)
            .context("failed leaving alternate screen for DSL prompt")?;
        disable_raw_mode().context("failed disabling raw mode for DSL prompt")?;
        self.active = false;
        Ok(())
    }

    fn resume_after_line_input(&mut self) -> Result<()> {
        if self.active {
            return Ok(());
        }
        enable_raw_mode().context("failed re-enabling raw mode for visual mode")?;
        execute!(std::io::stdout(), EnterAlternateScreen, Hide)
            .context("failed restoring alternate screen for visual mode")?;
        self.active = true;
        Ok(())
    }
}

impl Drop for VisualTerminalGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = execute!(std::io::stdout(), Show, LeaveAlternateScreen);
            let _ = disable_raw_mode();
        }
    }
}

impl VisualTerminal for VisualTerminalGuard {
    fn suspend_for_line_input(&mut self) -> Result<()> {
        Self::suspend_for_line_input(self)
    }
    fn resume_after_line_input(&mut self) -> Result<()> {
        Self::resume_after_line_input(self)
    }
    fn size(&self) -> Result<(u16, u16)> {
        Ok(terminal::size()?)
    }
    fn poll_event(&self) -> Result<Option<CrosstermEvent>> {
        if crossterm::event::poll(Duration::ZERO)? {
            Ok(Some(crossterm::event::read()?))
        } else {
            Ok(None)
        }
    }
    fn paint_lines(&mut self, lines: &[String]) -> Result<()> {
        let mut stdout = std::io::stdout().lock();
        queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;
        for (row, line) in lines.iter().enumerate() {
            queue!(
                stdout,
                MoveTo(0, u16::try_from(row).unwrap_or(u16::MAX)),
                Print(line)
            )?;
        }
        stdout.flush()?;
        Ok(())
    }
}
// The attach loop publishes its committed target at the start of each turn,
// after handling the preceding event. A playbook-owned terminal pairs that
// boundary with the exact event it delivered, including actions that keep the
// same target. Session changes alone cannot acknowledge an input action.
struct AcknowledgedAttachEvent {
    event: CrosstermEvent,
    action: Option<String>,
    completed: tokio::sync::oneshot::Sender<Uuid>,
}

struct PlaybookAttachTerminal {
    inner: HeadlessAttachTerminal,
    events: tokio::sync::mpsc::Receiver<AcknowledgedAttachEvent>,
    action: Option<String>,
    retained: bool,
    pending: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Uuid>>>,
}

impl Write for PlaybookAttachTerminal {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl AttachTerminal for PlaybookAttachTerminal {
    fn presentation_only(&self) -> bool {
        self.retained
    }
    fn take_owned_action(&mut self) -> Option<String> {
        self.action.take()
    }
    fn geometry(&self) -> crate::runtime::attach::input::TerminalGeometry {
        self.inner.geometry()
    }

    fn enter_attach_mode(&mut self, kitty: bool, mouse: bool, paste: bool) -> Result<bool> {
        self.inner.enter_attach_mode(kitty, mouse, paste)
    }

    fn next_event(&mut self) -> AttachTerminalEventFuture<'_> {
        Box::pin(async move {
            tokio::select! {
                event = self.events.recv() => {
                    let Some(event) = event else { return Ok(None); };
                    self.action = event.action;
                    *self.pending.lock().map_err(|_| anyhow::anyhow!("attach acknowledgment lock poisoned"))? = Some(event.completed);
                    Ok(Some(event.event))
                }
                event = self.inner.next_event() => event,
            }
        })
    }

    fn restore_after_attach_ui(&mut self) -> Result<()> {
        self.inner.restore_after_attach_ui()
    }

    fn set_attached_session_id(&self, session_id: Uuid) {
        self.inner.set_attached_session_id(session_id);
        if let Ok(mut pending) = self.pending.lock()
            && let Some(completed) = pending.take()
        {
            let _ = completed.send(session_id);
        }
    }

    #[cfg(test)]
    fn observe_scrollback(&self, active: bool) {
        self.inner.observe_scrollback(active);
    }

    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    fn detect_image_capabilities(&mut self) -> bmux_image::host_caps::HostImageCapabilities {
        self.inner.detect_image_capabilities()
    }
}

pub(super) struct RealAttachPlaybookRuntime {
    terminal: HeadlessAttachTerminalHandle,
    events: tokio::sync::mpsc::Sender<AcknowledgedAttachEvent>,
    input_resolved: std::sync::atomic::AtomicBool,
    prefix: String,
    required_frame_after: std::sync::atomic::AtomicU64,
    observer: Option<crate::runtime::attach::observer::AttachObserver>,
    task: tokio::task::JoinHandle<Result<crate::runtime::attach::runtime::AttachRunOutcome>>,
}

impl RealAttachPlaybookRuntime {
    fn ensure_input_resolved(&self) -> Result<()> {
        ensure!(
            self.input_resolved
                .load(std::sync::atomic::Ordering::Acquire),
            "real attachment has an uncommitted action; further input is suspended"
        );
        Ok(())
    }

    async fn send_event(&self, event: CrosstermEvent, deadline: Instant) -> Result<Uuid> {
        self.send_input(event, None, deadline).await
    }

    async fn send_input(
        &self,
        event: CrosstermEvent,
        action: Option<String>,
        deadline: Instant,
    ) -> Result<Uuid> {
        self.ensure_input_resolved()?;
        if let Some(snapshot) = self
            .observer
            .as_ref()
            .and_then(crate::runtime::attach::observer::AttachObserver::latest)
        {
            self.required_frame_after
                .store(snapshot.revision, std::sync::atomic::Ordering::Release);
        }
        self.input_resolved
            .store(false, std::sync::atomic::Ordering::Release);
        let (completed, completion) = tokio::sync::oneshot::channel();
        self.events
            .try_send(AcknowledgedAttachEvent {
                event,
                action,
                completed,
            })
            .map_err(|_| anyhow::anyhow!("real attachment exited before accepting input"))?;
        let committed =
            tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), completion)
                .await
                .context("real attachment action completion timed out; input target is unresolved")?
                .context("real attachment exited before committing the submitted action")?;
        self.input_resolved
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(committed)
    }

    #[cfg(test)]
    async fn send_chord(&self, chord: &str, deadline: Instant) -> Result<Uuid> {
        self.wait_until_ready().await?;
        let strokes = crate::input::parse_key_chord(chord)
            .map_err(|error| anyhow::anyhow!("invalid attach key chord '{chord}': {error}"))?;
        let mut committed_target = self
            .terminal
            .attached_session_id()
            .context("real attachment target missing")?;
        for stroke in strokes {
            committed_target = self
                .send_event(crossterm_event_from_stroke(stroke), deadline)
                .await?;
        }
        Ok(committed_target)
    }

    async fn wait_until_ready(&self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.terminal.attached_session_id().is_none() {
                ensure!(
                    !self.task.is_finished(),
                    "real attachment exited before becoming ready"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("real attachment readiness timed out")?
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.terminal.resize(cols, rows)
    }

    async fn detach(self) {
        let Self {
            terminal,
            events,
            mut task,
            ..
        } = self;
        // Close admission even with a modal prompt open: a synthesized detach
        // chord could otherwise become prompt text. This ends only our view.
        drop(events);
        let captured_bytes = terminal.output_bytes().len();
        if tokio::time::timeout(Duration::from_secs(2), &mut task)
            .await
            .is_err()
        {
            task.abort();
            let _ = task.await;
            warn!("owned attach detach deadline exceeded; task cancelled");
        } else {
            debug!(captured_bytes, "real-attach playbook runtime detached");
        }
    }
}

async fn start_cli_attach_runtime(
    attach_client: BmuxClient,
    session_id: Uuid,
    attach_config: Option<BmuxConfig>,
    paths: Option<ConfigPaths>,
    viewport: (u16, u16),
) -> Result<RealAttachPlaybookRuntime> {
    let prefix = attach_config.as_ref().map_or_else(
        || BmuxConfig::load().unwrap_or_default().keybindings.prefix,
        |config| config.keybindings.prefix.clone(),
    );
    let kernel_client_factory = paths.map(|paths| {
        let factory: crate::runtime::KernelClientFactory = std::sync::Arc::new(move || {
            let paths = paths.clone();
            Box::pin(async move {
                BmuxClient::connect_with_paths(&paths, "bmux-playbook-plugin")
                    .await
                    .map_err(Into::into)
            })
        });
        factory
    });
    let (inner, handle) = HeadlessAttachTerminal::new(viewport.0, viewport.1);
    let (events, event_rx) = tokio::sync::mpsc::channel(1);
    let managed = bmux_playbook::managed::ManagedRun::from_env()?.is_some();
    let (publisher, observer) = crate::runtime::attach::observer::attach_observer_channel();
    let retained_observer = managed.then_some(observer);
    let mut export = retained_observer.clone();
    let export_identity = if managed {
        Some((
            PathBuf::from(
                std::env::var_os("BMUX_PLAYBOOK_MANAGED_ROOT").context("managed root missing")?,
            ),
            std::env::var("BMUX_PLAYBOOK_MANAGED_GENERATION")?.parse::<u64>()?,
        ))
    } else {
        None
    };
    let mut terminal = PlaybookAttachTerminal {
        inner,
        events: event_rx,
        action: None,
        retained: managed,
        pending: std::sync::Mutex::new(None),
    };
    let target = session_id.to_string();
    let task = tokio::spawn(async move {
        let attach =
            crate::runtime::attach::runtime::run_session_attach_with_terminal_config_observer(
                attach_client,
                Some(target.as_str()),
                None,
                false,
                kernel_client_factory,
                &mut terminal,
                attach_config,
                managed.then_some(publisher),
            );
        let output = async move {
            if let (Some(observer), Some((root, generation))) = (export.as_mut(), export_identity) {
                loop {
                    let snapshot = observer
                        .changed()
                        .await
                        .context("managed attach output ended")?;
                    let frame =
                        crate::runtime::attach::observer::transport::project_frame(&snapshot)?;
                    let packet = bmux_playbook::managed_display::DisplayPacket {
                        generation,
                        epoch: snapshot.epoch,
                        session: snapshot.attached_id,
                        frame: bmux_web_renderer::encode_binary(&frame)?,
                    };
                    let root = root.clone();
                    tokio::task::spawn_blocking(move || packet.publish(&root)).await??;
                }
            }
            std::future::pending::<Result<()>>().await
        };
        tokio::select! {
            result = Box::pin(attach) => result,
            result = output => {
                result?;
                anyhow::bail!("managed attach export ended")
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok(RealAttachPlaybookRuntime {
        terminal: handle,
        events,
        input_resolved: std::sync::atomic::AtomicBool::new(true),
        prefix,
        required_frame_after: std::sync::atomic::AtomicU64::new(0),
        observer: retained_observer,
        task,
    })
}

impl RealAttach for RealAttachPlaybookRuntime {
    fn prefix_chord(&self) -> &str {
        &self.prefix
    }
    fn send_event(&self, event: CrosstermEvent, deadline: Instant) -> HostFuture<'_, Uuid> {
        Box::pin(Self::send_event(self, event, deadline))
    }
    fn send_action(&self, action: String, deadline: Instant) -> HostFuture<'_, Uuid> {
        Box::pin(async move {
            // Validate before admission; arbitrary malformed commands must not
            // be acknowledged as successful no-ops by the UI dispatcher.
            crate::input::parse_runtime_action_name(&action)?;
            self.send_input(CrosstermEvent::FocusGained, Some(action), deadline)
                .await
        })
    }
    fn ensure_input_resolved(&self) -> Result<()> {
        Self::ensure_input_resolved(self)
    }
    fn wait_until_ready(&self) -> HostFuture<'_, ()> {
        Box::pin(Self::wait_until_ready(self))
    }
    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        Self::resize(self, cols, rows)
    }
    fn attached_session_id(&self) -> Option<Uuid> {
        self.terminal.attached_session_id()
    }
    fn rendered_text(&self, rows: u16) -> Result<String> {
        if let Some(observer) = &self.observer {
            let snapshot = observer
                .latest()
                .context("owned attach has no committed frame")?;
            ensure!(
                snapshot.revision
                    > self
                        .required_frame_after
                        .load(std::sync::atomic::Ordering::Acquire),
                "owned attach has no post-input committed frame"
            );
            let buffer = snapshot.prepared.buffer();
            return Ok((buffer.area().y..buffer.area().bottom())
                .take(usize::from(rows))
                .filter_map(|row| buffer.row_symbols(row))
                .collect::<Vec<_>>()
                .join("\n"));
        }
        let grid = self.terminal.output_grid()?;
        Ok(bmux_playbook::screen::terminal_grid_to_text(
            grid.grid(),
            usize::from(rows),
        ))
    }
    fn drain_output(&self) -> Vec<Vec<u8>> {
        self.terminal.drain_output_chunks()
    }
    fn output_byte_count(&self) -> usize {
        self.terminal.output_bytes().len()
    }
    fn detach(self: Box<Self>) -> HostFuture<'static, ()> {
        Box::pin(async move {
            (*self).detach().await;
            Ok(())
        })
    }
}

struct CliSimulation(crate::runtime::attach::sim::AttachSimHarness);
impl AttachSimulation for CliSimulation {
    fn seed_pane_lines(&mut self, lines: &[&str], row: u16, col: u16) {
        self.0.seed_pane_lines(lines, row, col);
    }
    fn seed_vertical_split_panes(&mut self) {
        self.0.seed_vertical_split_panes();
    }
    fn seed_floating_pane_layout(&mut self) {
        self.0.seed_floating_pane_layout();
    }
    fn render(&mut self) {
        self.0.render();
    }
    fn resize_viewport(&mut self, cols: u16, rows: u16) {
        self.0.resize_viewport(cols, rows);
    }
    fn rendered(&self) -> &str {
        self.0.rendered()
    }
    fn send_attach_chord(&mut self, chord: &str) -> Result<Vec<String>> {
        self.0.send_attach_chord(chord)
    }
    fn help_overlay_open(&self) -> bool {
        self.0.help_overlay_open()
    }
    fn help_overlay_scroll(&self) -> usize {
        self.0.help_overlay_scroll()
    }
    fn prompt_active(&self) -> bool {
        self.0.prompt_active()
    }
    fn selected_text(&mut self) -> Option<String> {
        self.0.selected_text()
    }
    fn scrollback_active(&self) -> bool {
        self.0.scrollback_active()
    }
    fn selection_active(&self) -> bool {
        self.0.selection_active()
    }
    fn scrollback_cursor(&self) -> Option<(usize, usize)> {
        self.0.scrollback_cursor()
    }
    fn effect_operations(&self) -> Vec<&'static str> {
        self.0
            .effects()
            .iter()
            .map(|effect| match effect {
                crate::runtime::attach::state::AttachUiEffect::Resize { .. } => "resize-pane",
                crate::runtime::attach::state::AttachUiEffect::Focus { .. } => "focus-pane",
                crate::runtime::attach::state::AttachUiEffect::MoveFloating { .. } => {
                    "move-floating-pane"
                }
            })
            .collect()
    }
    fn locate_text(&self, text: &str) -> Option<bmux_playbook::host::LocatedText> {
        self.0
            .locate_text(text)
            .map(|location| bmux_playbook::host::LocatedText {
                start_col: usize::from(location.start_col),
                end_col: usize::from(location.end_col),
                center_col: usize::from(location.center_col),
                row: usize::from(location.row),
            })
    }
    fn mouse_event(&mut self, event: bmux_playbook::host::MouseEvent) -> Result<()> {
        use crate::runtime::attach::input::{
            TerminalModifiers, TerminalMouseButton, TerminalMouseEvent, TerminalMousePhase,
        };
        use bmux_playbook::host::{MouseButton, MousePhase};
        let phase = match event.phase {
            MousePhase::Down => TerminalMousePhase::Down,
            MousePhase::Up => TerminalMousePhase::Up,
            MousePhase::Drag => TerminalMousePhase::Drag,
            MousePhase::Move => TerminalMousePhase::Move,
            MousePhase::ScrollUp => TerminalMousePhase::ScrollUp,
            MousePhase::ScrollDown => TerminalMousePhase::ScrollDown,
            MousePhase::ScrollLeft => TerminalMousePhase::ScrollLeft,
            MousePhase::ScrollRight => TerminalMousePhase::ScrollRight,
        };
        let button = event.button.map(|button| match button {
            MouseButton::Left => TerminalMouseButton::Left,
            MouseButton::Right => TerminalMouseButton::Right,
            MouseButton::Middle => TerminalMouseButton::Middle,
        });
        self.0.send_mouse(TerminalMouseEvent {
            phase,
            button,
            col: event.col,
            row: event.row,
            modifiers: TerminalModifiers::default(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::sandbox::SandboxServer;
    use super::super::screen::ScreenInspector;
    use super::*;
    use bmux_playbook::subst::RuntimeVars;
    use std::collections::BTreeMap;
    async fn start_real_attach_playbook_runtime(
        sandbox: Option<&SandboxServer>,
        session: Uuid,
        _context: Option<Uuid>,
        viewport: (u16, u16),
    ) -> Result<RealAttachPlaybookRuntime> {
        let client = if let Some(sandbox) = sandbox {
            sandbox.connect("bmux-playbook-real-attach").await?
        } else {
            BmuxClient::connect_default("bmux-playbook-real-attach").await?
        };
        let config = sandbox
            .map(|sandbox| BmuxConfig::load_from_path(&sandbox.paths().config_file()))
            .transpose()?;
        start_cli_attach_runtime(
            client,
            session,
            config,
            sandbox.map(|sandbox| sandbox.paths().clone()),
            viewport,
        )
        .await
    }
    #[tokio::test]
    async fn owned_action_uses_the_same_acknowledgment_fence() {
        let (mut terminal, runtime) = acknowledged_test_runtime();
        let target = Uuid::new_v4();
        let action = "plugin:bmux.finder:show";
        assert_eq!(runtime.prefix_chord(), "ctrl+b");
        let send = runtime.send_action(action.into(), Instant::now() + Duration::from_secs(5));
        tokio::pin!(send);
        assert!(matches!(
            futures_util::poll!(&mut send),
            std::task::Poll::Pending
        ));
        terminal.next_event().await.unwrap().unwrap();
        assert_eq!(terminal.take_owned_action().as_deref(), Some(action));
        assert!(terminal.take_owned_action().is_none());
        assert!(runtime.ensure_input_resolved().is_err());
        terminal.set_attached_session_id(target);
        assert_eq!(send.await.unwrap(), target);
        assert!(runtime.ensure_input_resolved().is_ok());
        runtime.task.abort();
    }

    #[tokio::test]
    async fn closing_owned_admission_ends_view_even_with_other_terminal_senders() {
        let (mut terminal, runtime) = acknowledged_test_runtime();
        drop(runtime.events);
        assert!(terminal.next_event().await.unwrap().is_none());
        runtime.task.abort();
    }

    fn acknowledged_test_runtime() -> (PlaybookAttachTerminal, RealAttachPlaybookRuntime) {
        let (inner, handle) = HeadlessAttachTerminal::new(10, 3);
        let (events, event_rx) = tokio::sync::mpsc::channel(1);
        (
            PlaybookAttachTerminal {
                inner,
                events: event_rx,
                action: None,
                retained: false,
                pending: std::sync::Mutex::new(None),
            },
            RealAttachPlaybookRuntime {
                terminal: handle,
                events,
                input_resolved: std::sync::atomic::AtomicBool::new(true),
                prefix: "ctrl+b".into(),
                required_frame_after: std::sync::atomic::AtomicU64::new(0),
                observer: None,
                task: tokio::spawn(std::future::pending()),
            },
        )
    }

    #[tokio::test]
    async fn chord_submission_waits_for_initial_attachment() {
        let (mut terminal, runtime) = acknowledged_test_runtime();
        let target = Uuid::new_v4();
        let send = runtime.send_chord("a", Instant::now() + Duration::from_secs(5));
        tokio::pin!(send);
        assert!(matches!(
            futures_util::poll!(&mut send),
            std::task::Poll::Pending
        ));
        assert!(terminal.events.is_empty());
        terminal.set_attached_session_id(target);
        let driver = async {
            assert!(matches!(
                terminal.next_event().await.unwrap(),
                Some(CrosstermEvent::Key(_))
            ));
            terminal.set_attached_session_id(target);
        };
        let (result, ()) = tokio::join!(send, driver);
        result.unwrap();
        runtime.task.abort();
    }

    #[tokio::test]
    async fn action_completion_requires_its_own_delivery_boundary() {
        let (mut terminal, runtime) = acknowledged_test_runtime();
        let first = Uuid::new_v4();
        terminal.set_attached_session_id(first);
        let send = runtime.send_event(
            CrosstermEvent::Paste("first".into()),
            Instant::now() + Duration::from_secs(5),
        );
        tokio::pin!(send);
        assert!(matches!(
            futures_util::poll!(&mut send),
            std::task::Poll::Pending
        ));
        // An unrelated/previous retarget before delivery cannot complete this action.
        terminal.set_attached_session_id(Uuid::new_v4());
        assert!(matches!(
            futures_util::poll!(&mut send),
            std::task::Poll::Pending
        ));
        terminal.next_event().await.unwrap().unwrap();
        assert!(matches!(
            futures_util::poll!(&mut send),
            std::task::Poll::Pending
        ));
        terminal.set_attached_session_id(first);
        assert_eq!(send.await.unwrap(), first);

        let second = runtime.send_event(
            CrosstermEvent::Paste("second".into()),
            Instant::now() + Duration::from_secs(5),
        );
        tokio::pin!(second);
        assert!(matches!(
            futures_util::poll!(&mut second),
            std::task::Poll::Pending
        ));
        terminal.set_attached_session_id(first);
        assert!(matches!(
            futures_util::poll!(&mut second),
            std::task::Poll::Pending
        ));
        terminal.next_event().await.unwrap().unwrap();
        // Same-target UI actions complete too, without requiring a retarget.
        terminal.set_attached_session_id(first);
        assert_eq!(second.await.unwrap(), first);
        runtime.task.abort();
    }

    #[tokio::test]
    async fn uncommitted_action_times_out_and_disconnect_is_not_completion() {
        let (mut terminal, runtime) = acknowledged_test_runtime();
        let send = runtime.send_event(CrosstermEvent::Paste("late".into()), Instant::now());
        assert!(send.await.unwrap_err().to_string().contains("timed out"));
        terminal.next_event().await.unwrap().unwrap();
        // Late completion has only the expired action's receiver.
        terminal.set_attached_session_id(Uuid::new_v4());
        assert!(runtime.ensure_input_resolved().is_err());
        assert!(
            runtime
                .send_event(CrosstermEvent::Paste("blocked".into()), Instant::now())
                .await
                .unwrap_err()
                .to_string()
                .contains("suspended")
        );
        runtime.task.abort();

        let (mut terminal, runtime) = acknowledged_test_runtime();
        let send = runtime.send_event(
            CrosstermEvent::Paste("lost".into()),
            Instant::now() + Duration::from_secs(5),
        );
        tokio::pin!(send);
        assert!(matches!(
            futures_util::poll!(&mut send),
            std::task::Poll::Pending
        ));
        terminal.next_event().await.unwrap().unwrap();
        drop(terminal);
        assert!(
            send.await
                .unwrap_err()
                .to_string()
                .contains("exited before committing")
        );
        runtime.task.abort();
    }

    #[test]
    fn headless_screen_preserves_output_resize_order() {
        let (mut first, first_handle) = HeadlessAttachTerminal::new(10, 3);
        let (mut second, second_handle) = HeadlessAttachTerminal::new(20, 4);
        first.write_all(b"ABCDEFGHIJK").unwrap();
        second.write_all(b"independent").unwrap();
        first_handle.resize(20, 3).unwrap();
        first.write_all(b"\x1b[?1049hALT\x1b[?1049l!").unwrap();
        let mut expected = bmux_terminal_grid::TerminalGridStream::new(
            10,
            3,
            bmux_terminal_grid::GridLimits::default(),
        )
        .unwrap();
        expected.process(b"ABCDEFGHIJK");
        expected.resize(20, 3).unwrap();
        expected.process(b"\x1b[?1049hALT\x1b[?1049l!");
        assert_eq!(
            first_handle.output_screen_text(20, 3),
            bmux_terminal_grid::visible_text(expected.grid(), 0, 3)
        );
        assert!(
            second_handle
                .output_screen_text(20, 4)
                .contains("independent")
        );
        assert!(
            first_handle
                .output_screen_text(20, 3)
                .contains("ABCDEFGHIJK!")
        );
    }

    #[tokio::test]
    async fn control_retarget_commits_only_after_successful_open() {
        let executable = std::env::current_exe().unwrap();
        let binary = executable.parent().unwrap().parent().unwrap().join("bmux");
        let sandbox = SandboxServer::start(super::super::sandbox::SandboxStartOptions {
            shell: Some("sh"),
            plugin_config: &super::super::types::PluginConfig::default(),
            startup_timeout: Duration::from_secs(15),
            env: &BTreeMap::new(),
            env_mode: super::super::types::SandboxEnvMode::Clean,
            binary: Some(&binary),
            sandbox_config_file: None,
            bundled_plugin_ids: &[],
        })
        .await
        .unwrap();
        let result = async {
            let mut client = bmux_playbook::transport::ExecutionClient::new(
                sandbox.connect("retarget-control").await?,
                host(),
            );
            let first = typed_new_session_playbook(&mut client, None).await?;
            let second = typed_new_session_playbook(&mut client, None).await?;
            let mut current = None;
            let mut vars = RuntimeVars::new(BTreeMap::new());
            reconcile_real_attach_target(first, &mut client, &mut current, &mut vars).await?;
            assert_eq!(current, Some(first));
            // Model an observed target arriving after the preceding chord wait.
            reconcile_real_attach_target(second, &mut client, &mut current, &mut vars).await?;
            assert_eq!(current, Some(second));
            assert_eq!(vars.session_id, Some(second));
            let missing = Uuid::new_v4();
            assert!(
                reconcile_real_attach_target(missing, &mut client, &mut current, &mut vars)
                    .await
                    .is_err()
            );
            assert_eq!(current, Some(second));
            assert_eq!(vars.session_id, Some(second));
            client
                .attach_input(second, b"printf RETARGET_OK\\r".to_vec())
                .await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        sandbox.shutdown(false).await.unwrap();
        result.unwrap();
    }

    #[tokio::test]
    async fn simultaneous_attach_preserves_independent_screens() {
        let executable = std::env::current_exe().unwrap();
        let binary = executable.parent().unwrap().parent().unwrap().join("bmux");
        let sandbox = SandboxServer::start(super::super::sandbox::SandboxStartOptions {
            shell: Some("sh"),
            plugin_config: &super::super::types::PluginConfig::default(),
            startup_timeout: Duration::from_secs(15),
            env: &BTreeMap::new(),
            env_mode: super::super::types::SandboxEnvMode::Clean,
            binary: Some(&binary),
            sandbox_config_file: None,
            bundled_plugin_ids: &[],
        })
        .await
        .unwrap();
        // Exercise normal-mode UI commands explicitly; the product default is
        // passthrough insert mode, where prefixed bytes correctly reach the PTY.
        let config_path = sandbox.paths().config_file();
        let mut config = std::fs::read_to_string(&config_path).unwrap();
        config.push_str("\n[keybindings]\ninitial_mode = 'normal'\n");
        std::fs::write(&config_path, config).unwrap();
        let mut control = bmux_playbook::transport::ExecutionClient::new(
            sandbox.connect("replication-control").await.unwrap(),
            host(),
        );
        let session = typed_new_session_playbook(&mut control, None)
            .await
            .unwrap();
        let first = start_real_attach_playbook_runtime(Some(&sandbox), session, None, (80, 24))
            .await
            .unwrap();
        let second = start_real_attach_playbook_runtime(Some(&sandbox), session, None, (120, 30))
            .await
            .unwrap();
        let result = async {
            let grant = control.attach_grant(SessionSelector::ById(session)).await?;
            control.open_attach_stream_info(&grant).await?;
            control.attach_input(session, b"printf '\\033[2J\\033[HMAIN_RETAINED\\n\\033[?1049h\\033[2J\\033[HALT_ACTIVE'; read answer; printf '\\033[?1049l'; read answer\r".to_vec()).await?;
            wait_for_replication_screens(&first, &second, "ALT_ACTIVE", (80, 24)).await?;
            first.resize(90, 26)?;
            wait_for_replication_screens(&first, &second, "ALT_ACTIVE", (90, 26)).await?;
            control.attach_input(session, b"\r".to_vec()).await?;
            wait_for_replication_screens(&first, &second, "MAIN_RETAINED", (90, 26)).await?;
            for runtime in [&first, &second] {
                runtime.send_chord("ctrl+a [", Instant::now() + Duration::from_secs(5)).await?;
            }
            wait_for_scrollback_mode(&first, &second, true).await?;
            wait_for_replication_screens(&first, &second, "MAIN_RETAINED", (90, 26)).await?;
            for runtime in [&first, &second] {
                runtime.send_chord("Escape", Instant::now() + Duration::from_secs(5)).await?;
            }
            wait_for_scrollback_mode(&first, &second, false).await?;
            wait_for_replication_screens(&first, &second, "MAIN_RETAINED", (90, 26)).await?;
            let mut inspector = ScreenInspector::new(90, 26);
            inspector.refresh(&mut control, session).await?;
            ensure!(inspector.pane_scrollback_text(1).is_some_and(|text| text.contains("MAIN_RETAINED")), "authoritative scrollback lost restored content");
            ensure!(!first.task.is_finished() && !second.task.is_finished(), "attach loop exited");
            Ok::<_, anyhow::Error>(())
        }.await;
        first.task.abort();
        second.task.abort();
        let _ = tokio::join!(first.task, second.task);
        sandbox.shutdown(false).await.unwrap();
        result.unwrap();
    }

    #[tokio::test]
    async fn captured_history_window_reads_real_server_pin() {
        use bmux_pane_runtime_plugin_api::attach_runtime_state as api;
        let executable = std::env::current_exe().unwrap();
        let binary = executable.parent().unwrap().parent().unwrap().join("bmux");
        let sandbox = SandboxServer::start(super::super::sandbox::SandboxStartOptions {
            shell: Some("sh"),
            plugin_config: &super::super::types::PluginConfig::default(),
            startup_timeout: Duration::from_secs(15),
            env: &BTreeMap::new(),
            env_mode: super::super::types::SandboxEnvMode::Clean,
            binary: Some(&binary),
            sandbox_config_file: None,
            bundled_plugin_ids: &[],
        })
        .await
        .unwrap();
        let result = async {
            let mut client = bmux_playbook::transport::ExecutionClient::new(
                sandbox.connect("history-window-test").await?,
                host(),
            );
            let session = typed_new_session_playbook(&mut client, None).await?;
            let grant = client.attach_grant(SessionSelector::ById(session)).await?;
            client.open_attach_stream_info(&grant).await?;
            let pane = client.attach_layout(session).await?.panes[0].id;
            client
                .attach_input(
                    session,
                    b"i=0; while [ $i -lt 100 ]; do printf 'HISTORY_MARKER\\n'; i=$((i+1)); done\r"
                        .to_vec(),
                )
                .await?;
            let capture = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let capture =
                        api::client::attach_history_capture_v1(&mut client, session, pane)
                            .await?
                            .map_err(|err| anyhow::anyhow!("{err:?}"))?;
                    if capture.history_line_count > 40 {
                        break Ok::<_, anyhow::Error>(capture);
                    }
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
            })
            .await??;
            let pin = bmux_attach_pipeline::ScrollbackPin {
                capture: Some(bmux_attach_pipeline::ScrollbackCapture {
                    identity: capture.capture_id,
                    lines: capture.history_line_count,
                    truncated: capture.history_truncated,
                    width: capture.width,
                    height: capture.height,
                }),
                pin_id: capture.pin.pin_id,
                total_scrolled_rows: capture.pin.total_scrolled_rows,
                max_scrollback_offset: capture.pin.max_scrollback_offset as usize,
                stream_end: capture.pin.stream_end,
                created_epoch_secs: 0,
            };
            let window = crate::pane_runtime_client::captured_history_window(
                &mut client,
                session,
                pane,
                pin,
                usize::from(capture.height),
                2,
                (usize::from(capture.width), None, 0),
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("history window fell back"))?;
            ensure!(
                window.rows.iter().all(|row| bmux_terminal_grid::row_text(
                    row,
                    usize::from(capture.width)
                )
                .contains("HISTORY_MARKER")),
                "unexpected frozen rows"
            );
            api::client::attach_pane_scrollback_unpin(&mut client, session, pane, pin.pin_id)
                .await?
                .map_err(|err| anyhow::anyhow!("{err:?}"))?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        sandbox.shutdown(false).await.unwrap();
        result.unwrap();
    }

    #[tokio::test]
    async fn real_capture_fetch_recovers_scrolled_image_across_widths() {
        use crate::playbook::sandbox::{SandboxServer, SandboxStartOptions};
        use crate::runtime::attach::scrollback_fetch::{Request, fetch_with_client};
        use bmux_attach_pipeline::{CapturedHistoryAnchor, ScrollbackPin};
        use bmux_pane_runtime_plugin_api::attach_runtime_state as api;
        let executable = std::env::current_exe().unwrap();
        let binary = executable.parent().unwrap().parent().unwrap().join("bmux");
        let sandbox = SandboxServer::start(SandboxStartOptions {
            shell: Some("sh"),
            plugin_config: &crate::playbook::types::PluginConfig::default(),
            startup_timeout: std::time::Duration::from_secs(15),
            env: &std::collections::BTreeMap::new(),
            env_mode: crate::playbook::types::SandboxEnvMode::Clean,
            binary: Some(&binary),
            sandbox_config_file: None,
            bundled_plugin_ids: &[],
        })
        .await
        .unwrap();
        let result = async {
            let mut client = bmux_playbook::transport::ExecutionClient::new(sandbox.connect("history-image-fetch-test").await?, host());
            // Use the baseline session created by the sandbox through its public CLI.
            let session = bmux_playbook::engine::typed_new_session_playbook(&mut client, None).await?;
            let grant = client.attach_grant(SessionSelector::ById(session)).await?;
            client.open_attach_stream_info(&grant).await?;
            let pane = client.attach_layout(session).await?.panes[0].id;
            client.attach_input(session, b"printf '\\033[2J\\033[H'; printf '%050d' 0; printf '\\033Pq\"1;1;1;6#1;2;100;0;0~\\033\\\\MARK'; i=0; while [ $i -lt 60 ]; do printf '\\r\\nROW'; i=$((i+1)); done\r".to_vec()).await?;
            let capture = tokio::time::timeout(std::time::Duration::from_secs(15), async {
                loop {
                    let capture = api::client::attach_history_capture_v1(&mut client, session, pane).await?
                        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
                    if capture.history_line_count > 30 { break Ok::<_, anyhow::Error>(capture); }
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                }
            }).await??;
            let pin = ScrollbackPin {
                capture: Some(bmux_attach_pipeline::ScrollbackCapture {
                    identity: capture.capture_id, lines: capture.history_line_count,
                    truncated: capture.history_truncated, width: capture.width, height: capture.height,
                }),
                pin_id: capture.pin.pin_id, total_scrolled_rows: capture.pin.total_scrolled_rows,
                max_scrollback_offset: capture.pin.max_scrollback_offset as usize,
                stream_end: capture.pin.stream_end, created_epoch_secs: 0,
            };
            let cache = std::sync::Arc::default();
            let mut recovered = false;
            for width in [80, 40, 120, 80] {
                // Locate the image in retained history without depending on shell startup rows.
                for line in 0..capture.history_line_count {
                    let request = Request {
                        boundary: None,
                        session, pane, pin: Some(pin), offset: 1, width, rows: 1,
                        total: None, anchor: Some(CapturedHistoryAnchor {
                            capture_id: capture.capture_id, line_index: u32::try_from(line)?, column: if width == 40 { 40 } else { 0 },
                        }), delta: 0,
                    };
                    let window = fetch_with_client(&mut client, request, std::sync::Arc::clone(&cache))
                        .await.map_err(anyhow::Error::msg)?;
                    if !window.images.is_empty() {
                        assert_eq!(window.images[0].position_row, 0);
                        let expected_column = 50 % width;
                        assert_eq!(usize::from(window.images[0].position_col), expected_column);
                        assert_eq!(window.rows[0].cells()[expected_column].text(), "M");
                        assert!(!window.images[0].raw_data.is_empty());
                        #[cfg(feature = "image-sixel")]
                        assert_recovered_image_emits_at_viewport_origin(&window);
                        #[cfg(feature = "image-sixel")]
                        assert_recovered_image_in_frame(session, pane, pin, window);
                        recovered = true;
                        break;
                    }
                }
                anyhow::ensure!(recovered, "historical image not recovered at width {width}");
                recovered = false;
            }
            Ok::<_, anyhow::Error>(())
        }.await;
        sandbox.shutdown(false).await.unwrap();
        result.unwrap();
    }

    #[cfg(feature = "image-sixel")]
    fn assert_recovered_image_in_frame(
        session: Uuid,
        pane: Uuid,
        pin: bmux_attach_pipeline::ScrollbackPin,
        window: bmux_attach_pipeline::PaneScrollbackWindow,
    ) {
        use crate::runtime::attach::{
            input::TerminalGeometry,
            runtime::{DisplayCaptureFanout, render_attach_frame_to_writer},
            state::{AttachDirtySource, AttachViewState},
        };
        use bmux_attach_layout_protocol::{
            AttachFocusTarget, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
            PaneLayoutNode,
        };
        let width = u16::try_from(window.projection_width).unwrap();
        let image_column = window.images[0].position_col;
        let rect = AttachRect {
            x: 0,
            y: 0,
            w: width,
            h: 3,
        };
        let layout = bmux_client::AttachLayoutState {
            context_id: None,
            session_id: session,
            focused_pane_id: pane,
            panes: Vec::new(),
            layout_root: PaneLayoutNode::Leaf { pane_id: pane },
            zoomed: false,
            scene: AttachScene {
                session_id: session,
                focus: AttachFocusTarget::Pane { pane_id: pane },
                surfaces: vec![AttachSurface {
                    id: pane,
                    kind: AttachSurfaceKind::Pane,
                    layer: bmux_attach_layout_protocol::AttachLayer::Pane,
                    z: 0,
                    rect,
                    content_rect: AttachRect { h: 1, ..rect },
                    interactive_regions: Vec::new(),
                    opaque: true,
                    visible: true,
                    accepts_input: true,
                    cursor_owner: false,
                    pane_id: Some(pane),
                }],
            },
        };
        let mut state = AttachViewState::new(bmux_client::AttachOpenInfo {
            session_id: session,
            context_id: None,
            can_write: true,
        });
        state.host_image_caps = bmux_image::host_caps::HostImageCapabilities {
            sixel: true,
            ..Default::default()
        };
        state.pane_scrollback.insert(
            pane,
            bmux_attach_pipeline::PaneScrollbackView {
                offset: window.scrollback_offset,
                cursor: bmux_attach_pipeline::AttachScrollbackCursor { row: 0, col: 0 },
                selection_anchor: None,
                captured_selection: None,
                pin: Some(pin),
            },
        );
        state
            .pane_buffers
            .entry(pane)
            .or_default()
            .scrollback_window = Some(window);
        state.dirty.mark_full_frame(AttachDirtySource::Scrollback);
        let mut output = Vec::new();
        render_attach_frame_to_writer(
            &mut output,
            &mut state,
            &layout,
            &bmux_appearance::RuntimeAppearance::default(),
            &[],
            0,
            &bmux_config::DamageBehaviorConfig::default(),
            u64::MAX,
            &mut DisplayCaptureFanout::default(),
            TerminalGeometry {
                cols: width,
                rows: 3,
            },
            None,
        )
        .unwrap();
        assert!(output.windows(2).any(|bytes| bytes == b"\x1bP"));
        assert!(
            output.windows(4).any(|bytes| bytes == b"MARK"),
            "attachment frame omitted captured text"
        );
        let cursor = format!("\x1b[1;{}H", image_column + 1);
        assert!(
            output
                .windows(cursor.len())
                .any(|bytes| bytes == cursor.as_bytes())
        );
        assert!(state.pane_images_presented);
    }

    #[cfg(feature = "image-sixel")]
    fn assert_recovered_image_emits_at_viewport_origin(
        window: &bmux_attach_pipeline::PaneScrollbackWindow,
    ) {
        let images = window
            .images
            .iter()
            .map(bmux_image::PaneImage::from)
            .collect::<Vec<_>>();
        let mut output = Vec::new();
        bmux_image::compositor::render_pane_images_clipped(
            &mut output,
            &images,
            bmux_image::compositor::PaneRect {
                x: 5,
                y: 7,
                w: u16::try_from(window.projection_width).unwrap(),
                h: 1,
            },
            &[],
            &bmux_image::host_caps::HostImageCapabilities {
                sixel: true,
                ..Default::default()
            },
            &mut bmux_image::compositor::KittyHostState::default(),
        )
        .unwrap();
        let cursor = format!("\x1b[8;{}H", 6 + window.images[0].position_col);
        assert!(
            output
                .windows(cursor.len())
                .any(|bytes| bytes == cursor.as_bytes())
        );
        assert!(
            output.windows(2).any(|bytes| bytes == b"\x1bP"),
            "missing recovered Sixel output"
        );
    }

    async fn wait_for_scrollback_mode(
        first: &RealAttachPlaybookRuntime,
        second: &RealAttachPlaybookRuntime,
        active: bool,
    ) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            while first.terminal.scrollback_active() != active
                || second.terminal.scrollback_active() != active
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("scrollback mode transition timed out")
    }

    async fn wait_for_replication_screens(
        first: &RealAttachPlaybookRuntime,
        second: &RealAttachPlaybookRuntime,
        marker: &str,
        size: (u16, u16),
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let a = first.terminal.output_screen_text(size.0, size.1);
            let b = second.terminal.output_screen_text(120, 30);
            if a.contains(marker) && b.contains(marker) {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "missing {marker}: first={a:?}, second={b:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn attach_input_uses_default_modal_window_bindings() {
        let mut config = BmuxConfig::default();
        "normal".clone_into(&mut config.keybindings.initial_mode);
        let mut processor = CliHost.key_processor(&config);
        let stroke = crate::input::parse_key_chord("c").unwrap()[0];
        assert_eq!(
            processor.process_stroke(stroke),
            vec![RuntimeAction::PluginCommand {
                plugin_id: "bmux.tabs".to_owned(),
                command_name: "new-tab".to_owned(),
                args: Vec::new(),
            }]
        );
    }

    #[test]
    fn attach_input_maps_ctrl_s_to_next_window_plugin_command() {
        let mut config = BmuxConfig::default();
        "normal".clone_into(&mut config.keybindings.initial_mode);
        let mut processor = CliHost.key_processor(&config);
        let stroke = crate::input::parse_key_chord("ctrl+s").unwrap()[0];
        assert_eq!(
            processor.process_stroke(stroke),
            vec![RuntimeAction::PluginCommand {
                plugin_id: "bmux.tabs".to_owned(),
                command_name: "next-tab".to_owned(),
                args: Vec::new(),
            }]
        );
    }
}
