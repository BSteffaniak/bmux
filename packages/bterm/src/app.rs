//! Native application lifecycle. The main thread owns the surface; an attach
//! worker owns BMUX orchestration. Neither window closure nor output failure
//! sends a server shutdown or destroys a plugin-owned resource. Clipboard
//! ownership is application-lifetime; only reducer requests cross its bounded
//! UI broker. Rejected presentation retries at most three times with backoff,
//! without acknowledging output or advancing committed interaction state.

use anyhow::{Context, Result, bail};
use bterm_native_platform::{
    PlatformError, PlatformEvent, WindowApplication, WindowContext, WindowMetrics, WindowOptions,
};
use bterm_native_renderer::{NativeRenderer, RendererOptions};
use clap::Parser;
use std::path::PathBuf;

use crate::runtime_selection::RuntimeSelection;
use crate::window::{NativeOutput, NativeOutputReceiver, output_channel};
use bmux_frontend_runtime::presentation::PresentationIdentity;

impl Arguments {
    /// Capture complete runtime intent once. An external root bypasses bundle
    /// discovery entirely and never reads the user's default BMUX roots.
    pub fn resolve(&self) -> Result<RuntimeSelection> {
        if let Some(root) = &self.runtime_root {
            return RuntimeSelection::resolve(Some(root.clone()), PathBuf::new(), PathBuf::new());
        }
        let companion = match &self.companion {
            Some(path) => path.clone(),
            None => std::env::current_exe()
                .context("locate Bterm bundle")?
                .parent()
                .context("Bterm executable has no bundle directory")?
                .join(if cfg!(windows) { "bmux.exe" } else { "bmux" }),
        };
        let root = match &self.managed_root {
            Some(root) => root.clone(),
            None => default_managed_root()?,
        };
        RuntimeSelection::resolve(None, companion, root)
    }
}

fn default_managed_root() -> Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .context("set --managed-root or LOCALAPPDATA")?
    } else if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(path)
    } else {
        PathBuf::from(std::env::var_os("HOME").context("set --managed-root or HOME")?)
            .join(".local/state")
    };
    if !base.is_absolute() {
        bail!("managed state base must be absolute; set --managed-root");
    }
    Ok(base.join("bterm/bmux"))
}

/// Explicit process intent. External roots are attach-only and never fall back.
#[derive(Debug, Parser)]
#[command(name = "bterm", version, about = "Native BMUX window")]
pub struct Arguments {
    /// Attach only to this complete runtime root (config/runtime/data/state).
    #[arg(long, conflicts_with_all = ["companion", "managed_root"])]
    pub runtime_root: Option<PathBuf>,
    /// Verified BMUX executable accompanied by bterm-bundle.json.
    #[arg(long)]
    pub companion: Option<PathBuf>,
    /// Complete persistent root for the managed companion.
    #[arg(long)]
    pub managed_root: Option<PathBuf>,
    /// Existing BMUX attachment target; omission uses BMUX's normal selection.
    #[arg(long)]
    pub session: Option<String>,
}

/// Existing BMUX attach integration, not a second resource manager. Implementors
/// must run publication off the window thread and route native input through
/// the canonical bindings and committed retained scene.
pub trait AttachWorker: 'static {
    fn start(&mut self, output: NativeOutput, viewport: PresentationViewport) -> Result<()>;
    fn event(&mut self, event: PlatformEvent, viewport: PresentationViewport) -> Result<()>;
    fn retry_presentation(&mut self, viewport_generation: u64) -> Result<()>;
    fn detach(&mut self);
    fn check(&mut self) -> Result<bool>;
    /// Pixel mapping becomes usable only after the corresponding full output.
    /// Mapped pointer events retain this complete identity until canonical dequeue;
    /// viewport and cell metrics alone cannot authorize queued interaction.
    fn presented(
        &mut self,
        identity: PresentationIdentity,
        viewport: PresentationViewport,
        cell: [f32; 2],
    );
    fn invalidate_presentation(&mut self);
    fn accessibility(
        &mut self,
        identity: bmux_frontend_runtime::presentation::PresentationIdentity,
        action: bterm_native_platform::accessibility::AccessibilityAction,
    ) -> Result<()>;
}

/// Projection geometry only. This value never directly resizes an execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentationViewport {
    pub columns: u16,
    pub rows: u16,
    pub generation: u64,
}

impl PresentationViewport {
    fn advance_generation(&mut self) -> Result<()> {
        self.generation = self
            .generation
            .checked_add(1)
            .context("presentation viewport generation exhausted")?;
        Ok(())
    }
}

#[derive(Default)]
struct RetryBudget {
    attempts: u8,
    due: Option<std::time::Instant>,
}

impl RetryBudget {
    fn schedule(&mut self, now: std::time::Instant) -> Option<std::time::Duration> {
        if self.due.is_some() {
            return Some(std::time::Duration::ZERO);
        }
        if self.attempts >= 3 {
            return None;
        }
        let delay = std::time::Duration::from_millis(50 * (1_u64 << self.attempts));
        self.attempts += 1;
        self.due = Some(now + delay);
        Some(delay)
    }

    fn take_due(&mut self, now: std::time::Instant) -> bool {
        if self.due.is_some_and(|due| now >= due) {
            self.due = None;
            true
        } else {
            false
        }
    }
}

/// Window lifecycle driver. The worker can publish only while a live surface is
/// available; minimized and suspended windows reject output without a receipt.
pub struct Application<W: AttachWorker> {
    worker: W,
    runtime: tokio::runtime::Runtime,
    renderer: Option<NativeRenderer>,
    output: Option<NativeOutputReceiver>,
    viewport: PresentationViewport,
    active: bool,
    semantic_generation: u64,
    last_output: Option<bmux_frontend_runtime::presentation::PresentationIdentity>,
    retained_output: Option<bmux_frontend_runtime::presentation::PreparedDelivery>,
    metrics: Option<WindowMetrics>,
    retry: RetryBudget,
    clipboard: Result<
        bterm_native_platform::clipboard::Clipboard,
        bterm_native_platform::clipboard::ClipboardError,
    >,
}

impl<W: AttachWorker> Application<W> {
    /// Construct without opening a window or starting a companion.
    pub fn new(worker: W) -> Result<Self> {
        Ok(Self {
            worker,
            runtime: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("create native frontend executor")?,
            renderer: None,
            output: None,
            viewport: PresentationViewport {
                columns: 1,
                rows: 1,
                generation: 0,
            },
            active: false,
            semantic_generation: 0,
            last_output: None,
            retained_output: None,
            metrics: None,
            retry: RetryBudget::default(),
            clipboard: bterm_native_platform::clipboard::Clipboard::new(),
        })
    }

    fn resize(&mut self, metrics: WindowMetrics) -> Result<()> {
        if self.metrics == Some(metrics) {
            return Ok(());
        }
        self.worker.invalidate_presentation();
        self.retry = RetryBudget::default();
        let renderer = self
            .renderer
            .as_mut()
            .context("native renderer is unavailable")?;
        renderer.resize(metrics.physical_size);
        renderer.set_scale_factor(metrics.scale_factor as f32)?;
        let [width, height] = renderer.cell_size();
        let columns = projection_cells(metrics.physical_size.width, width)?;
        let rows = projection_cells(metrics.physical_size.height, height)?;
        self.metrics = Some(metrics);
        self.viewport.columns = columns;
        self.viewport.rows = rows;
        self.viewport.advance_generation()?;
        Ok(())
    }

    fn suspend_projection(&mut self, context: &mut WindowContext<'_>) -> Result<()> {
        self.active = false;
        self.worker.invalidate_presentation();
        context.invalidate_accessibility();
        self.retry = RetryBudget::default();
        self.viewport.advance_generation()?;
        self.worker.event(PlatformEvent::Suspended, self.viewport)?;
        self.reject_pending("native surface suspended");
        Ok(())
    }

    fn dispatch(&mut self, context: &mut WindowContext<'_>, event: PlatformEvent) -> Result<()> {
        if !matches!(event, PlatformEvent::CloseRequested)
            && let Some(output) = &self.output
        {
            output.service_clipboard(&mut self.clipboard);
        }
        match event {
            PlatformEvent::Resumed => {
                context.set_ime_enabled(true);
                let metrics = context.metrics();
                let renderer = self
                    .runtime
                    .block_on(NativeRenderer::new(
                        context.surface_target(),
                        RendererOptions {
                            size: metrics.physical_size,
                            scale_factor: metrics.scale_factor as f32,
                            ..RendererOptions::default()
                        },
                    ))
                    .context("initialize native renderer")?;
                self.renderer = Some(renderer);
                self.resize(metrics)?;
                self.active = true;
                if self.output.is_none() {
                    let (output, receiver) = output_channel(context.proxy());
                    self.output = Some(receiver);
                    self.worker.start(output, self.viewport)?;
                } else {
                    self.worker.event(PlatformEvent::Resumed, self.viewport)?;
                }
                if metrics.physical_size.is_empty() {
                    self.suspend_projection(context)?;
                }
                context.request_redraw();
            }
            PlatformEvent::Suspended => {
                self.suspend_projection(context)?;
                self.metrics = None;
                self.renderer = None;
            }
            PlatformEvent::Resized(metrics) => {
                self.worker.invalidate_presentation();
                context.invalidate_accessibility();
                if self.renderer.is_some() {
                    self.resize(metrics)?;
                }
                if metrics.physical_size.is_empty() || self.renderer.is_none() {
                    self.suspend_projection(context)?;
                } else {
                    self.active = true;
                    self.worker
                        .event(PlatformEvent::Resized(metrics), self.viewport)?;
                }
                context.request_redraw();
            }
            PlatformEvent::CloseRequested => {
                if let Some(output) = &self.output {
                    output.close();
                }
                self.worker.detach();
                context.exit();
            }
            PlatformEvent::Wake | PlatformEvent::RedrawRequested => {
                if self.worker.check()? {
                    context.exit();
                    return Ok(());
                }
                if self.retry.take_due(std::time::Instant::now()) {
                    self.worker.retry_presentation(self.viewport.generation)?;
                }
                if let Some(output) = &self.output {
                    if let Some(pending) = output.try_recv() {
                        let active = self.active;
                        let metrics = context.metrics();
                        let viewport = self.viewport;
                        let renderer = &mut self.renderer;
                        let worker = &mut self.worker;
                        let semantic_generation = &mut self.semantic_generation;
                        let last_output = &mut self.last_output;
                        let retained_output = &mut self.retained_output;
                        let accepted = pending.present(|delivery| {
                            if !active || metrics.physical_size.is_empty() {
                                Err("native surface is not drawable".into())
                            } else if delivery.identity.viewport_generation != viewport.generation {
                                Err("native presentation viewport is stale".into())
                            } else if last_output.is_some_and(|last| {
                                last.epoch != delivery.identity.epoch
                                    || last.revision >= delivery.identity.revision
                            }) {
                                Err("native presentation revision is stale".into())
                            } else {
                                let renderer = renderer
                                    .as_mut()
                                    .ok_or_else(|| "native renderer unavailable".to_string())?;
                                renderer
                                    .present(&delivery.prepared)
                                    .map_err(|error| error.to_string())?;
                                *last_output = Some(delivery.identity);
                                *retained_output = Some(delivery.clone());
                                let cell = renderer.cell_size();
                                worker.presented(delivery.identity, viewport, cell);
                                *semantic_generation = semantic_generation.saturating_add(1);
                                if let Err(error) = context.commit_accessibility(
                                    *semantic_generation,
                                    delivery.prepared.semantics(),
                                    bterm_native_platform::accessibility::AccessibilityGeometry {
                                        clip: delivery.prepared.buffer().area(),
                                        origin_x: 0.0,
                                        origin_y: 0.0,
                                        cell_width: f64::from(cell[0]),
                                        cell_height: f64::from(cell[1]),
                                    },
                                ) {
                                    context.invalidate_accessibility();
                                    eprintln!(
                                        "bterm: accessibility projection rejected: {error:?}"
                                    );
                                }
                                if let Some(cursor) =
                                    delivery.prepared.cursor().filter(|cursor| cursor.visible)
                                {
                                    context.set_ime_cursor_area(
                                        bterm_native_platform::PhysicalRect {
                                            x: (f32::from(cursor.position.x) * cell[0]) as i32,
                                            y: (f32::from(cursor.position.y) * cell[1]) as i32,
                                            width: cell[0].ceil() as u32,
                                            height: cell[1].ceil() as u32,
                                        },
                                    );
                                }
                                Ok(())
                            }
                        });
                        if accepted {
                            self.retry = RetryBudget::default();
                        } else if self.active && !context.metrics().physical_size.is_empty() {
                            if let Some(delay) = self.retry.schedule(std::time::Instant::now()) {
                                let proxy = context.proxy();
                                let _retry_wake = self.runtime.spawn(async move {
                                    tokio::time::sleep(delay).await;
                                    let _ = proxy.wake();
                                });
                            } else {
                                bail!("native presentation recovery exhausted");
                            }
                        }
                    } else if self.active
                        && !context.metrics().physical_size.is_empty()
                        && let (Some(renderer), Some(retained)) =
                            (&mut self.renderer, &self.retained_output)
                        && retained.identity.viewport_generation == self.viewport.generation
                    {
                        // Exposure redraws replay the actual retained output;
                        // they never acknowledge a new interaction revision.
                        renderer.present(&retained.prepared)?;
                    }
                }
            }
            event => self.worker.event(event, self.viewport)?,
        }
        if let Some(last) = self.last_output {
            let actions = context
                .take_accessibility_actions()
                .map_err(|error| anyhow::anyhow!("native accessibility input: {error:?}"))?;
            for action in actions {
                if action.generation == self.semantic_generation {
                    self.worker.accessibility(last, action)?;
                }
            }
        }
        Ok(())
    }

    fn reject_pending(&self, reason: &str) {
        if let Some(output) = &self.output {
            while let Some(pending) = output.try_recv() {
                pending.present(|_| Err(reason.into()));
            }
        }
    }
}

impl<W: AttachWorker> WindowApplication for Application<W> {
    fn event(
        &mut self,
        context: &mut WindowContext<'_>,
        event: PlatformEvent,
    ) -> Result<(), PlatformError> {
        self.dispatch(context, event)
            .map_err(|error| PlatformError::Application(format!("{error:#}")))
    }
}

impl<W: AttachWorker> Drop for Application<W> {
    fn drop(&mut self) {
        if let Some(output) = &self.output {
            output.close();
        }
        self.worker.detach();
    }
}

/// Resolve intent, connect through the verified startup boundary, then run the
/// real native host on this process's main thread.
///
/// # Errors
/// Selection, compatibility, attach, and graphics failures remain visible.
pub fn run() -> Result<()> {
    let arguments = Arguments::parse();
    let selection = arguments.resolve()?;
    let config = selection
        .roots()
        .load_config(&bmux_config::ConfigLoadOverrides::default())?;
    run_window(BmuxWorker {
        selection,
        config: Some(config),
        target: arguments.session.unwrap_or_default(),
        handle: None,
        thread: None,
        result: None,
        committed_geometry: None,
        pointer: None,
        drag: None,
        scroll_remainder: [0.0; 2],
        modifiers: bmux_frontend_runtime::input::Modifiers::default(),
    })
}

struct BmuxWorker {
    selection: RuntimeSelection,
    config: Option<bmux_config::BmuxConfig>,
    target: String,
    handle: Option<bmux_cli::attach::NativeAttachHandle>,
    thread: Option<std::thread::JoinHandle<()>>,
    result: Option<std::sync::mpsc::Receiver<Result<()>>>,
    committed_geometry: Option<(PresentationIdentity, PresentationViewport, [f32; 2])>,
    pointer: Option<bmux_frontend_runtime::input::PhysicalPoint>,
    drag: Option<bmux_frontend_runtime::input::PointerButton>,
    scroll_remainder: [f64; 2],
    modifiers: bmux_frontend_runtime::input::Modifiers,
}

impl AttachWorker for BmuxWorker {
    fn start(&mut self, output: NativeOutput, viewport: PresentationViewport) -> Result<()> {
        let delivery = output.clone();
        let (handle, mut host) = bmux_cli::attach::NativeAttachHost::channel(
            viewport.columns,
            viewport.rows,
            viewport.generation,
            move |prepared| delivery.present(prepared),
        )?;
        let clipboard = output.clone();
        host.set_config_paths(self.selection.roots().config_paths());
        host.set_clipboard(move |request| clipboard.clipboard_request(request));
        self.handle = Some(handle);
        let selection = self.selection.clone();
        let config = self
            .config
            .take()
            .context("BMUX frontend was already started")?;
        let target = self.target.clone();
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        self.result = Some(result_rx);
        self.thread = Some(
            std::thread::Builder::new()
                .name("bterm-attach".into())
                .spawn(move || {
                    let result = (|| -> Result<()> {
                        let runtime = tokio::runtime::Builder::new_multi_thread()
                            .enable_all()
                            .build()?;
                        runtime.block_on(async {
                            bmux_cli::attach::initialize_native_runtime_state(
                                &config,
                                &selection.roots().config_paths(),
                            )?;
                            let client =
                                crate::runtime_launch::connect_selection(&selection).await?;
                            bmux_cli::attach::run_native_with_client(client, &target, config, host)
                                .await?;
                            Ok(())
                        })
                    })();
                    let _ = result_tx.send(result);
                    let _ = output.wake();
                })
                .context("start BMUX attachment worker")?,
        );
        Ok(())
    }

    fn event(&mut self, event: PlatformEvent, viewport: PresentationViewport) -> Result<()> {
        use bmux_cli::attach::NativeAttachEvent;
        let event = match event {
            PlatformEvent::Input(bmux_frontend_runtime::input::InputEvent::PointerMoved {
                position,
            }) => {
                self.pointer = Some(position);
                match self.committed_geometry {
                    Some((identity, committed, cell))
                        if committed.generation == viewport.generation =>
                    {
                        pointer_cell(position, committed, cell).map(|(column, row)| {
                            NativeAttachEvent::Motion {
                                identity,
                                column,
                                row,
                                drag: self.drag,
                                modifiers: self.modifiers,
                            }
                        })
                    }
                    _ => None,
                }
            }
            PlatformEvent::Input(bmux_frontend_runtime::input::InputEvent::PointerLeft) => {
                self.pointer = None;
                None
            }
            PlatformEvent::Input(bmux_frontend_runtime::input::InputEvent::PointerButton {
                button,
                phase,
                modifiers,
            }) => {
                self.modifiers = modifiers;
                self.drag = if phase == bmux_frontend_runtime::input::KeyPhase::Release {
                    None
                } else {
                    Some(button)
                };
                match (self.committed_geometry, self.pointer) {
                    (Some((identity, committed, cell)), Some(pointer))
                        if committed.generation == viewport.generation =>
                    {
                        pointer_cell(pointer, committed, cell).map(|(column, row)| {
                            NativeAttachEvent::Mouse {
                                identity,
                                column,
                                row,
                                button,
                                phase,
                                modifiers,
                            }
                        })
                    }
                    _ => None,
                }
            }
            PlatformEvent::Input(bmux_frontend_runtime::input::InputEvent::Scroll {
                delta,
                modifiers,
            }) => {
                self.modifiers = modifiers;
                match (self.committed_geometry, self.pointer) {
                    (Some((identity, committed, cell)), Some(pointer))
                        if committed.generation == viewport.generation =>
                    {
                        pointer_cell(pointer, committed, cell).and_then(|(column, row)| {
                            let (x, y) = match delta {
                                bmux_frontend_runtime::input::ScrollDelta::Lines { x, y } => (x, y),
                                bmux_frontend_runtime::input::ScrollDelta::Pixels { x, y } => {
                                    (x / f64::from(cell[0]), y / f64::from(cell[1]))
                                }
                            };
                            let (delta_x, delta_y) =
                                accumulate_scroll(&mut self.scroll_remainder, x, y);
                            (delta_x != 0 || delta_y != 0).then_some(NativeAttachEvent::Scroll {
                                identity,
                                column,
                                row,
                                delta_x,
                                delta_y,
                                modifiers,
                            })
                        })
                    }
                    _ => None,
                }
            }
            PlatformEvent::Input(bmux_frontend_runtime::input::InputEvent::Key(key)) => {
                self.modifiers = key.modifiers;
                Some(NativeAttachEvent::Input(
                    bmux_frontend_runtime::input::InputEvent::Key(key),
                ))
            }
            PlatformEvent::PasteRequested => Some(NativeAttachEvent::PasteFromClipboard),
            PlatformEvent::Input(input) => Some(NativeAttachEvent::Input(input)),
            PlatformEvent::Focused(focused) => Some(NativeAttachEvent::Input(
                bmux_frontend_runtime::input::InputEvent::Focus(focused),
            )),
            PlatformEvent::Suspended => Some(NativeAttachEvent::Suspend {
                viewport_generation: viewport.generation,
            }),
            PlatformEvent::Resized(_) | PlatformEvent::Resumed => Some(NativeAttachEvent::Resize {
                columns: viewport.columns,
                rows: viewport.rows,
                viewport_generation: viewport.generation,
            }),
            _ => None,
        };
        if let (Some(handle), Some(event)) = (&self.handle, event) {
            handle.try_send(event)?;
        }
        Ok(())
    }

    fn accessibility(
        &mut self,
        identity: bmux_frontend_runtime::presentation::PresentationIdentity,
        action: bterm_native_platform::accessibility::AccessibilityAction,
    ) -> Result<()> {
        if let Some(handle) = &self.handle {
            handle.try_send(bmux_cli::attach::NativeAttachEvent::Accessibility {
                epoch: identity.epoch,
                attached_id: identity.attached_id,
                viewport_generation: identity.viewport_generation,
                revision: identity.revision,
                semantic_id: action.semantic_id,
                action: action.action,
            })?;
        }
        Ok(())
    }

    fn presented(
        &mut self,
        identity: PresentationIdentity,
        viewport: PresentationViewport,
        cell: [f32; 2],
    ) {
        if identity.viewport_generation != viewport.generation {
            self.invalidate_presentation();
            return;
        }
        self.committed_geometry = Some((identity, viewport, cell));
    }

    fn invalidate_presentation(&mut self) {
        self.committed_geometry = None;
        self.drag = None;
        self.scroll_remainder = [0.0; 2];
    }

    fn retry_presentation(&mut self, viewport_generation: u64) -> Result<()> {
        if let Some(handle) = &self.handle {
            handle.try_send(bmux_cli::attach::NativeAttachEvent::RetryPresentation {
                viewport_generation,
            })?;
        }
        Ok(())
    }

    fn detach(&mut self) {
        self.committed_geometry = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.try_send(bmux_cli::attach::NativeAttachEvent::Close);
        }
        // Dropping the join handle is deliberate: never block the native main
        // thread on a provider or stop a persistent runtime as a window side effect.
        self.thread.take();
    }

    fn check(&mut self) -> Result<bool> {
        if let Some(result) = &self.result {
            match result.try_recv() {
                Ok(result) => {
                    self.result = None;
                    result?;
                    return Ok(true);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    bail!("BMUX attachment worker exited without a result")
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        Ok(false)
    }
}

/// Open the real platform host with an existing BMUX frontend integration.
pub fn run_window(worker: impl AttachWorker) -> Result<()> {
    bterm_native_platform::run(WindowOptions::default(), Application::new(worker)?)
        .context("run native window")
}

fn accumulate_scroll(remainder: &mut [f64; 2], x: f64, y: f64) -> (i16, i16) {
    let mut steps = [0_i16; 2];
    for (axis, delta) in [x, y].into_iter().enumerate() {
        if !delta.is_finite() {
            continue;
        }
        let total = remainder[axis] + delta;
        let whole = total
            .trunc()
            .clamp(f64::from(i16::MIN), f64::from(i16::MAX));
        steps[axis] = whole as i16;
        remainder[axis] = (total - whole).clamp(-1.0, 1.0);
    }
    (steps[0], steps[1])
}

fn pointer_cell(
    point: bmux_frontend_runtime::input::PhysicalPoint,
    viewport: PresentationViewport,
    cell: [f32; 2],
) -> Option<(u16, u16)> {
    if !point.x.is_finite()
        || !point.y.is_finite()
        || point.x < 0.0
        || point.y < 0.0
        || !cell.iter().all(|value| value.is_finite() && *value > 0.0)
    {
        return None;
    }
    let column = (point.x / f64::from(cell[0])).floor();
    let row = (point.y / f64::from(cell[1])).floor();
    if column >= f64::from(viewport.columns) || row >= f64::from(viewport.rows) {
        return None;
    }
    Some((column as u16, row as u16))
}

fn projection_cells(pixels: u32, cell: f32) -> Result<u16> {
    if !cell.is_finite() || cell <= 0.0 {
        bail!("invalid native font cell metrics");
    }
    let count = (f64::from(pixels) / f64::from(cell))
        .floor()
        .clamp(1.0, f64::from(u16::MAX));
    Ok(count as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_selection_conflicts_with_launch_authority() {
        assert!(
            Arguments::try_parse_from(["bterm", "--runtime-root", "/a", "--companion", "/bmux"])
                .is_err()
        );
        assert!(
            Arguments::try_parse_from(["bterm", "--runtime-root", "/a", "--managed-root", "/b"])
                .is_err()
        );
        assert!(Arguments::try_parse_from(["bterm", "--runtime-root", "/a"]).is_ok());
    }

    fn worker_without_runtime() -> BmuxWorker {
        BmuxWorker {
            selection: RuntimeSelection::External {
                roots: crate::runtime_paths::RuntimeRoots::from_root(std::env::temp_dir()).unwrap(),
            },
            config: None,
            target: String::new(),
            handle: None,
            thread: None,
            result: None,
            committed_geometry: None,
            pointer: None,
            drag: None,
            scroll_remainder: [0.0; 2],
            modifiers: bmux_frontend_runtime::input::Modifiers::default(),
        }
    }

    #[test]
    fn suspend_and_resume_generations_are_fresh_and_never_wrap() {
        let mut viewport = PresentationViewport {
            columns: 80,
            rows: 24,
            generation: 5,
        };
        let displayed_generation = viewport.generation;
        viewport.advance_generation().unwrap();
        let suspended_generation = viewport.generation;
        viewport.advance_generation().unwrap();
        assert!(suspended_generation > displayed_generation);
        assert!(viewport.generation > suspended_generation);
        viewport.generation = u64::MAX;
        assert!(viewport.advance_generation().is_err());
        assert_eq!(viewport.generation, u64::MAX);
    }

    #[test]
    fn pointer_geometry_retains_complete_accepted_identity_and_invalidates_on_resize() {
        let mut worker = worker_without_runtime();
        let viewport = PresentationViewport {
            columns: 80,
            rows: 24,
            generation: 7,
        };
        let identity = PresentationIdentity {
            epoch: "11111111-1111-1111-1111-111111111111".parse().unwrap(),
            attached_id: "22222222-2222-2222-2222-222222222222".parse().unwrap(),
            revision: 19,
            viewport_generation: 7,
        };
        worker.presented(identity, viewport, [10.0, 20.0]);
        assert_eq!(
            worker.committed_geometry,
            Some((identity, viewport, [10.0, 20.0]))
        );
        worker.invalidate_presentation();
        assert!(worker.committed_geometry.is_none());
        worker.presented(
            PresentationIdentity {
                viewport_generation: 6,
                ..identity
            },
            viewport,
            [10.0, 20.0],
        );
        assert!(worker.committed_geometry.is_none());
    }

    #[test]
    fn retry_budget_is_backed_off_bounded_and_never_due_early() {
        let now = std::time::Instant::now();
        let mut budget = RetryBudget::default();
        for millis in [50, 100, 200] {
            let delay = std::time::Duration::from_millis(millis);
            assert_eq!(budget.schedule(now), Some(delay));
            assert!(!budget.take_due(now));
            assert!(budget.take_due(now + delay));
            assert!(!budget.take_due(now + delay));
        }
        assert_eq!(budget.schedule(now), None);
    }

    #[test]
    fn fractional_scroll_accumulates_and_rejects_nonfinite_input() {
        let mut remainder = [0.0; 2];
        assert_eq!(accumulate_scroll(&mut remainder, 0.4, -0.4), (0, 0));
        assert_eq!(accumulate_scroll(&mut remainder, 0.8, -0.8), (1, -1));
        assert_eq!(
            accumulate_scroll(&mut remainder, f64::NAN, f64::INFINITY),
            (0, 0)
        );
    }

    #[test]
    fn pointer_projection_requires_visible_committed_cell_bounds() {
        use bmux_frontend_runtime::input::PhysicalPoint;
        let viewport = PresentationViewport {
            columns: 80,
            rows: 24,
            generation: 2,
        };
        assert_eq!(
            pointer_cell(PhysicalPoint { x: 15.0, y: 31.0 }, viewport, [10.0, 20.0]),
            Some((1, 1))
        );
        assert_eq!(
            pointer_cell(PhysicalPoint { x: 800.0, y: 1.0 }, viewport, [10.0, 20.0]),
            None
        );
        assert_eq!(
            pointer_cell(PhysicalPoint { x: -1.0, y: 1.0 }, viewport, [10.0, 20.0]),
            None
        );
        assert_eq!(
            pointer_cell(
                PhysicalPoint {
                    x: f64::NAN,
                    y: 1.0
                },
                viewport,
                [10.0, 20.0]
            ),
            None
        );
    }

    #[test]
    fn projection_is_bounded_and_does_not_confuse_pixels_with_cells() {
        assert_eq!(projection_cells(960, 10.0).unwrap(), 96);
        assert_eq!(projection_cells(0, 10.0).unwrap(), 1);
        assert_eq!(projection_cells(u32::MAX, 1.0).unwrap(), u16::MAX);
        assert!(projection_cells(100, f32::NAN).is_err());
        assert!(projection_cells(100, 0.0).is_err());
    }
}
