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
use clap::{Parser, Subcommand};
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
        let companion = self.companion_path()?;
        let root = match &self.managed_root {
            Some(root) => root.clone(),
            None => default_managed_root()?,
        };
        RuntimeSelection::resolve(None, companion, root)
    }

    fn companion_path(&self) -> Result<PathBuf> {
        match &self.companion {
            Some(path) => Ok(path.clone()),
            None => Ok(std::env::current_exe()
                .context("locate Bterm bundle")?
                .parent()
                .context("Bterm executable has no bundle directory")?
                .join(if cfg!(windows) { "bmux.exe" } else { "bmux" })),
        }
    }
}

fn native_pointer_icon(
    pointer: bmux_attach_pipeline::pointer_shape::ResolvedPointerShape,
) -> Option<bterm_native_platform::CursorIcon> {
    use bmux_attach_pipeline::pointer_shape::ResolvedPointerShape;
    use bmux_terminal_protocol::pointer_shape::PointerShape;
    use bterm_native_platform::CursorIcon;
    let ResolvedPointerShape::Shape(shape) = pointer else {
        return None;
    };
    Some(match shape {
        PointerShape::Alias => CursorIcon::Alias,
        PointerShape::Cell => CursorIcon::Cell,
        PointerShape::Copy => CursorIcon::Copy,
        PointerShape::Crosshair => CursorIcon::Crosshair,
        PointerShape::Default => CursorIcon::Default,
        PointerShape::EResize => CursorIcon::EResize,
        PointerShape::EwResize => CursorIcon::EwResize,
        PointerShape::Grab => CursorIcon::Grab,
        PointerShape::Grabbing => CursorIcon::Grabbing,
        PointerShape::Help => CursorIcon::Help,
        PointerShape::Move => CursorIcon::Move,
        PointerShape::NResize => CursorIcon::NResize,
        PointerShape::NeResize => CursorIcon::NeResize,
        PointerShape::NeswResize => CursorIcon::NeswResize,
        PointerShape::NoDrop => CursorIcon::NoDrop,
        PointerShape::NotAllowed => CursorIcon::NotAllowed,
        PointerShape::NsResize => CursorIcon::NsResize,
        PointerShape::NwResize => CursorIcon::NwResize,
        PointerShape::NwseResize => CursorIcon::NwseResize,
        PointerShape::Pointer => CursorIcon::Pointer,
        PointerShape::Progress => CursorIcon::Progress,
        PointerShape::SResize => CursorIcon::SResize,
        PointerShape::SeResize => CursorIcon::SeResize,
        PointerShape::SwResize => CursorIcon::SwResize,
        PointerShape::Text => CursorIcon::Text,
        PointerShape::VerticalText => CursorIcon::VerticalText,
        PointerShape::WResize => CursorIcon::WResize,
        PointerShape::Wait => CursorIcon::Wait,
        PointerShape::ZoomIn => CursorIcon::ZoomIn,
        PointerShape::ZoomOut => CursorIcon::ZoomOut,
    })
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
    /// Validated sandbox execution descriptor; never an implicit host fallback.
    #[arg(long, conflicts_with_all = ["runtime_root", "managed_root", "companion"])]
    pub execution_context: Option<PathBuf>,
    /// Explicit named slot; attach only, never launch or replace its companion.
    #[arg(long, requires = "slots_manifest", conflicts_with_all = ["runtime_root", "managed_root", "companion", "execution_context"])]
    pub slot: Option<String>,
    /// Explicit absolute slot manifest used only with --slot.
    #[arg(long, requires = "slot")]
    pub slots_manifest: Option<PathBuf>,
    /// Bterm configuration overlay (below all selected BMUX sources).
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// BMUX configuration overlay (highest BMUX file priority).
    #[arg(long)]
    pub bmux_config: Option<PathBuf>,
    /// Do not discover the host BMUX configuration family.
    #[arg(long)]
    pub no_bmux_config: bool,
    /// Resolve scoped/project configuration for this target kind.
    #[arg(long)]
    pub config_scope: Option<String>,
    /// Select a composition profile using the existing BMUX resolver.
    #[arg(long)]
    pub config_profile: Option<String>,
    #[command(subcommand)]
    pub command: Option<ConfigurationCommand>,
    /// Select an active local Linux graphical login when display settings are absent.
    #[arg(long)]
    pub desktop_session: Option<String>,
    /// Existing BMUX attachment target; omission uses BMUX's normal selection.
    #[arg(long)]
    pub session: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum ConfigurationCommand {
    /// Start or reuse the exact managed companion without opening a window.
    #[command(hide = true)]
    ManagedConnect,
    /// Read-only configuration inspection; never opens a companion or window.
    Config {
        #[command(subcommand)]
        command: ConfigurationAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigurationAction {
    /// Explain selected configuration sources, safe settings, and execution paths.
    Explain,
}

/// Existing BMUX attach integration, not a second resource manager. Implementors
/// must run publication off the window thread and route native input through
/// the canonical bindings and committed retained scene.
pub trait AttachWorker: 'static {
    /// A coalesced native-only reload from the captured configuration path.
    fn take_native_configuration(&mut self) -> Option<crate::configuration::NativeConfiguration> {
        None
    }
    /// Optional diagnostics; never used for input/output authority.
    fn input_queue_metrics(&self) -> Option<bmux_cli::attach::QueueMetricsSnapshot> {
        None
    }
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
    typography: crate::window_typography::WindowTypography,
    applied_typography: Option<bterm_native_renderer::text::RendererTypography>,
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
            applied_typography: None,
            typography: crate::window_typography::WindowTypography::new(Default::default(), None)?,
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

    /// Construct with captured native policy. Does not open a window or write preferences.
    pub fn configured(
        worker: W,
        native: crate::configuration::NativeConfiguration,
        store: Option<crate::zoom_preferences::PreferenceStore>,
    ) -> Result<Self> {
        let mut application = Self::new(worker)?;
        application.typography = crate::window_typography::WindowTypography::new(native, store)?;
        Ok(application)
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
        let requested = self
            .typography
            .resolved(std::time::Instant::now(), metrics.scale_factor as f32);
        apply_window_typography(
            renderer,
            requested.clone(),
            metrics.scale_factor as f32,
            metrics.physical_size,
        )?;
        self.applied_typography = Some(requested);
        renderer.resize(metrics.physical_size);
        let [width, height] = renderer.cell_size();
        let columns = projection_cells(metrics.physical_size.width, width)?;
        let rows = projection_cells(metrics.physical_size.height, height)?;
        self.metrics = Some(metrics);
        self.viewport.columns = columns;
        self.viewport.rows = rows;
        self.viewport.advance_generation()?;
        Ok(())
    }

    fn update_typography(
        &mut self,
        context: &mut WindowContext<'_>,
        now: std::time::Instant,
    ) -> Result<()> {
        let next = self
            .typography
            .resolved(now, context.metrics().scale_factor as f32);
        if self.active
            && !context.metrics().physical_size.is_empty()
            && let Some(renderer) = &mut self.renderer
            && self.applied_typography.as_ref() != Some(&next)
        {
            // Even a size change that leaves rows/columns unchanged supersedes
            // queued output and its interaction geometry.
            apply_window_typography(
                renderer,
                next.clone(),
                context.metrics().scale_factor as f32,
                context.metrics().physical_size,
            )?;
            self.applied_typography = Some(next);
            let cell = renderer.cell_size();
            let metrics = context.metrics();
            self.viewport.columns = projection_cells(metrics.physical_size.width, cell[0])?;
            self.viewport.rows = projection_cells(metrics.physical_size.height, cell[1])?;
            self.viewport.advance_generation()?;
            self.worker.invalidate_presentation();
            context.invalidate_accessibility();
            self.last_output = None;
            self.retained_output = None;
            self.retry = RetryBudget::default();
            self.worker
                .event(PlatformEvent::Resized(metrics), self.viewport)?;
            context.request_redraw();
        }
        if self.active && self.typography.animating(now) {
            context.request_redraw_at(now + std::time::Duration::from_millis(16));
        }
        Ok(())
    }

    fn suspend_projection(&mut self, context: &mut WindowContext<'_>) -> Result<()> {
        self.active = false;
        self.worker.invalidate_presentation();
        context.invalidate_accessibility();
        // A suspended surface no longer owns a visible pointer target. In
        // particular, do not leave an OSC 22 hidden cursor active on resume
        // while waiting for the next committed presentation.
        context.set_pointer_icon(native_pointer_icon(
            bmux_attach_pipeline::pointer_shape::ResolvedPointerShape::Shape(
                bmux_terminal_protocol::pointer_shape::PointerShape::Default,
            ),
        ));
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
        let now = std::time::Instant::now();
        let consumed = match &event {
            PlatformEvent::Input(input) => self.typography.input(input, now)?,
            PlatformEvent::Focused(false) | PlatformEvent::Suspended => {
                self.typography.reset_input();
                false
            }
            _ => false,
        };
        self.typography.poll_save(now);
        if let Some(native) = self.worker.take_native_configuration() {
            let dpi = context.metrics().scale_factor as f32;
            let renderer = &mut self.renderer;
            if let Err(error) = self.typography.reconfigure(native, now, dpi, |typography| {
                if let Some(renderer) = renderer {
                    apply_window_typography(
                        renderer,
                        typography.clone(),
                        dpi,
                        context.metrics().physical_size,
                    )?;
                }
                Ok(())
            }) {
                eprintln!("bterm: native typography reload rejected: {error}");
            } else if self.renderer.is_some() {
                // Renderer validation above is transactional. Force the same
                // generation transition even if the candidate has equal metrics.
                self.metrics = None;
                self.resize(context.metrics())?;
                self.worker.invalidate_presentation();
                context.invalidate_accessibility();
                self.last_output = None;
                self.retained_output = None;
                self.worker
                    .event(PlatformEvent::Resized(context.metrics()), self.viewport)?;
            }
            context.request_redraw();
        }
        let visual_pending = self.renderer.is_some() && {
            self.applied_typography.as_ref()
                != Some(
                    &self
                        .typography
                        .resolved(now, context.metrics().scale_factor as f32),
                )
        };
        let geometry_committed = self
            .last_output
            .is_some_and(|identity| identity.viewport_generation == self.viewport.generation);
        if (consumed || matches!(event, PlatformEvent::RedrawRequested)) && geometry_committed {
            self.update_typography(context, now)?;
        } else if self.active && (visual_pending || self.typography.animating(now)) {
            // Coalesce target changes while one generation is being prepared.
            // Do not continuously supersede a slow compositor's pending output.
            context.request_redraw_at(now + std::time::Duration::from_millis(16));
        }
        if self.typography.save_pending() {
            context.request_redraw_at(now + std::time::Duration::from_millis(50));
        }
        if consumed {
            return Ok(());
        }
        match event {
            PlatformEvent::Resumed => {
                context.set_ime_enabled(true);
                let metrics = context.metrics();
                let mut renderer = self
                    .runtime
                    .block_on(NativeRenderer::new(
                        context.surface_target(),
                        RendererOptions {
                            size: metrics.physical_size,
                            scale_factor: metrics.scale_factor as f32,
                            font_size: self
                                .typography
                                .resolved(std::time::Instant::now(), metrics.scale_factor as f32)
                                .font_size,
                        },
                    ))
                    .context("initialize native renderer")?;
                renderer.set_typography(
                    self.typography
                        .resolved(std::time::Instant::now(), metrics.scale_factor as f32),
                )?;
                renderer.set_telemetry_enabled(
                    std::env::var_os("BTERM_RENDER_TELEMETRY").is_some_and(|value| value == "1"),
                );
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
                // Resume already installed these metrics before starting the
                // attachment. Winit may repeat them immediately (and later).
                // Do not revoke a valid presentation or resend its generation.
                if !projection_resize_needed(self.metrics, metrics, self.active) {
                    context.request_redraw();
                    return Ok(());
                }
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
                self.typography.flush();
                if let Some(output) = &self.output {
                    output.close();
                }
                self.worker.detach();
                context.exit();
            }
            event @ (PlatformEvent::Wake | PlatformEvent::RedrawRequested) => {
                if self.worker.check()? {
                    self.typography.flush();
                    context.exit();
                    return Ok(());
                }
                if self.retry.take_due(std::time::Instant::now()) {
                    self.worker.retry_presentation(self.viewport.generation)?;
                }
                if let Some(output) = &self.output {
                    if let Some(pending) = output.try_recv() {
                        let superseded =
                            pending.identity().viewport_generation < self.viewport.generation;
                        let active = self.active;
                        let metrics = context.metrics();
                        let viewport = self.viewport;
                        let renderer = &mut self.renderer;
                        let worker = &mut self.worker;
                        let semantic_generation = &mut self.semantic_generation;
                        let last_output = &mut self.last_output;
                        let retained_output = &mut self.retained_output;
                        let accepted = pending.present_with_telemetry(|delivery, frame_telemetry| {
                            if !active || metrics.physical_size.is_empty() {
                                Err("native surface is not drawable".into())
                            } else if delivery.identity.viewport_generation != viewport.generation {
                                Err(format!(
                                    "native presentation viewport is stale: received {}, current {}",
                                    delivery.identity.viewport_generation, viewport.generation,
                                ))
                            } else if !allocation_matches_viewport(
                                delivery.prepared.buffer().area().x,
                                delivery.prepared.buffer().area().y,
                                delivery.prepared.buffer().area().width,
                                delivery.prepared.buffer().area().height,
                                viewport,
                            ) {
                                Err("native presentation cell allocation does not match the viewport".into())
                            } else if last_output.is_some_and(|last| {
                                last.epoch != delivery.identity.epoch
                                    || last.revision >= delivery.identity.revision
                            }) {
                                Err(format!(
                                    "native presentation revision is stale: received {:?}, last displayed {:?}",
                                    delivery.identity, last_output,
                                ))
                            } else {
                                let renderer = renderer
                                    .as_mut()
                                    .ok_or_else(|| "native renderer unavailable".to_string())?;
                                let rendered = renderer.present(&delivery.prepared);
                                *frame_telemetry = renderer.last_frame_telemetry();
                                rendered.map_err(|error| error.to_string())?;
                                *last_output = Some(delivery.identity);
                                if let Some(pointer) = delivery.pointer_shape {
                                    context.set_pointer_icon(native_pointer_icon(pointer));
                                }
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
                                            x: (f32::from(cursor.position.x) * cell[0]).floor() as i32,
                                            y: (f32::from(cursor.position.y) * cell[1]).floor() as i32,
                                            width: ((f32::from(cursor.position.x) + 1.0) * cell[0]).ceil() as u32
                                                - (f32::from(cursor.position.x) * cell[0]).floor() as u32,
                                            height: ((f32::from(cursor.position.y) + 1.0) * cell[1]).ceil() as u32
                                                - (f32::from(cursor.position.y) * cell[1]).floor() as u32,
                                        },
                                    );
                                }
                                Ok(())
                            }
                        });
                        // Receipt has been delivered before optional diagnostic IO.
                        // Numeric values only: UUIDs are exact decimal u128 identities.
                        if let Some((attempts, sample)) = output.latest_telemetry()
                            && attempts.is_multiple_of(120)
                        {
                            let frame = sample.renderer.unwrap_or_default();
                            let (mode, latency) = self
                                .renderer
                                .as_ref()
                                .map_or((0, 0), NativeRenderer::presentation_config);
                            let refresh = context.nominal_refresh_millihertz();
                            if let Some(queue) = self
                                .worker
                                .input_queue_metrics()
                                .filter(|queue| queue.enabled)
                            {
                                eprintln!(
                                    "bterm_input_queue admitted={} dequeued={} full={} closed={} cancelled={} discarded={} pending={} high_water={} payload_bytes={} payload_high_water={} admission_p99_ns={} residence_p50_ns={} residence_p95_ns={} residence_p99_ns={}",
                                    queue.admitted,
                                    queue.dequeued,
                                    queue.rejected_full,
                                    queue.rejected_closed,
                                    queue.rejected_cancelled,
                                    queue.discarded,
                                    queue.pending,
                                    queue.pending_high_water,
                                    queue.payload_bytes,
                                    queue.payload_bytes_high_water,
                                    queue.admission_wait.p99_ns,
                                    queue.queue_residence.p50_ns,
                                    queue.queue_residence.p95_ns,
                                    queue.queue_residence.p99_ns
                                );
                            }
                            eprintln!(
                                "bterm_presentation attempts={} epoch={} revision={} attached={} viewport={} success={} gate_ns={} queue_ns={} output_ns={} renderer_sample={} renderer_ns={} text_ns={} image_ns={} acquire_ns={} encode_submit_ns={} gpu_wait_ns={} validation_ns={} present_call_ns={} present_mode={} max_frame_latency={} refresh_known={} nominal_refresh_millihertz={}",
                                attempts,
                                sample.identity.epoch.as_u128(),
                                sample.identity.revision,
                                sample.identity.attached_id.as_u128(),
                                sample.identity.viewport_generation,
                                u8::from(sample.succeeded),
                                sample.producer_gate_ns,
                                sample.queue_ns,
                                sample.output_ns,
                                u8::from(sample.renderer.is_some()),
                                frame.total_ns,
                                frame.text_cpu_ns,
                                frame.image_cpu_ns,
                                frame.surface_acquire_ns,
                                frame.gpu_encode_submit_ns,
                                frame.gpu_wait_ns,
                                frame.gpu_validation_ns,
                                frame.present_call_ns,
                                mode,
                                latency,
                                u8::from(refresh.is_some()),
                                refresh.unwrap_or(0),
                            );
                        }
                        if let Err(error) = accepted {
                            // A resize supersedes queued output without indicating a
                            // renderer failure. Reject its receipt, but let the queued
                            // geometry update converge without spending recovery retries.
                            if !superseded
                                && self.active
                                && !context.metrics().physical_size.is_empty()
                            {
                                if let Some(delay) = self.retry.schedule(std::time::Instant::now())
                                {
                                    eprintln!(
                                        "bterm: {error}; retry {}/3 in {}ms (viewport={}, surface={:?})",
                                        self.retry.attempts,
                                        delay.as_millis(),
                                        self.viewport.generation,
                                        context.metrics().physical_size,
                                    );
                                    let proxy = context.proxy();
                                    let _retry_wake = self.runtime.spawn(async move {
                                        tokio::time::sleep(delay).await;
                                        let _ = proxy.wake();
                                    });
                                } else {
                                    bail!(
                                        "native presentation recovery exhausted after 3 retries: {error}"
                                    );
                                }
                            }
                        } else {
                            self.retry = RetryBudget::default();
                        }
                    } else if replay_retained(
                        &event,
                        self.active,
                        !context.metrics().physical_size.is_empty(),
                        self.retained_output
                            .as_ref()
                            .map(|output| output.identity.viewport_generation),
                        self.viewport.generation,
                    ) && let (Some(renderer), Some(retained)) =
                        (&mut self.renderer, &self.retained_output)
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
                let _ = pending.present(|_| Err(reason.into()));
            }
        }
    }
}

impl<W: AttachWorker> WindowApplication for Application<W> {
    fn ready_to_show(&self) -> bool {
        // A hidden Metal surface cannot acquire a drawable. Show after successful
        // native initialization, not after the first GPU presentation. Visibility
        // grants no interaction authority: only successful output sets last_output.
        self.active
    }

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
    let (configuration, sandbox, slot) = arguments.capture_configuration()?;
    if arguments.command.is_none()
        && let Some(context) = &sandbox
    {
        let current: std::collections::BTreeMap<_, _> = std::env::vars_os().collect();
        if current != context.environment.0 {
            let mut command = std::process::Command::new(std::env::current_exe()?);
            command.args(std::env::args_os().skip(1));
            command.env_clear().envs(context.environment.0.iter());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt as _;
                return Err(command.exec()).context("enter selected sandbox environment");
            }
            #[cfg(not(unix))]
            {
                let status = command
                    .status()
                    .context("enter selected sandbox environment")?;
                anyhow::ensure!(status.success(), "sandbox application exited: {status}");
                return Ok(());
            }
        }
    }
    #[cfg(target_os = "linux")]
    if arguments.command.is_none() {
        crate::desktop::prepare(arguments.desktop_session.as_deref(), sandbox.is_some())?;
    }
    let selection = if let Some(context) = &sandbox {
        RuntimeSelection::External {
            roots: sandbox_roots(context),
        }
    } else if let Some(slot) = &slot {
        RuntimeSelection::External {
            roots: crate::runtime_paths::RuntimeRoots {
                config_dir: slot.config_paths.config_dir.clone(),
                runtime_dir: slot.config_paths.runtime_dir.clone(),
                data_dir: slot.config_paths.data_dir.clone(),
                state_dir: slot.config_paths.state_dir.clone(),
            },
        }
    } else if let Some(root) = &arguments.runtime_root {
        RuntimeSelection::External {
            roots: crate::runtime_paths::RuntimeRoots::from_root(root.clone())?,
        }
    } else {
        RuntimeSelection::resolve_configured(
            None,
            arguments.companion_path()?,
            configuration.inputs.paths.managed_roots()?,
            &configuration,
        )?
    };
    if matches!(arguments.command, Some(ConfigurationCommand::Config { .. })) {
        let roots = selection.roots();
        let mut output = configuration.effective_json();
        output["execution"] = serde_json::json!({
            "mode": if sandbox.is_some() { "sandbox-attach" } else if arguments.slot.is_some() { "slot-attach" } else if arguments.runtime_root.is_some() { "external" } else { "managed" },
            "config_dir": roots.config_dir,
            "runtime_dir": roots.runtime_dir,
            "data_dir": roots.data_dir,
            "state_dir": roots.state_dir,
            "sandbox_id": sandbox.as_ref().map(|context| &context.sandbox_id),
            "managed_identity": selection.managed_identity(),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }
    if matches!(
        arguments.command,
        Some(ConfigurationCommand::ManagedConnect)
    ) {
        anyhow::ensure!(
            sandbox.is_none() && slot.is_none(),
            "managed-connect requires managed runtime selection"
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let report = runtime.block_on(crate::headless::run(&selection, &configuration))?;
        println!("{}", serde_json::to_string(&report)?);
        return Ok(());
    }
    let event_loop = bterm_native_platform::NativeEventLoop::prepare()
        .context("connect native desktop before companion startup")?;
    let decorations = configuration.native.window.decorations;
    let native = configuration.native.clone();
    let preference_target = match &selection {
        RuntimeSelection::Managed { identity, .. } => format!(
            "managed:{}:{}",
            identity.durable_roots.state_dir.display(),
            identity.scope_id,
        ),
        RuntimeSelection::External { roots } => format!("external:{}", roots.runtime_dir.display()),
    };
    let preferences = crate::zoom_preferences::PreferenceStore::for_configuration(
        &configuration,
        &preference_target,
    )?;
    // Complete compatibility/readiness admission before creating even a hidden
    // native surface. Keep the executor that owns the client's transport tasks.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create companion connection executor")?;
    let (client, preparation) = runtime.block_on(
        crate::runtime_launch::prepare_native_configured(&selection, &configuration),
    )?;
    run_window_prepared(
        event_loop,
        BmuxWorker {
            preparation,
            prepared: Some(PreparedConnection { client, runtime }),
            selection,
            config: Some(configuration.bmux.clone()),
            configuration: Some(configuration),
            native_reload: Default::default(),
            frozen_configuration: sandbox.is_some(),
            target: arguments.session.unwrap_or_default(),
            handle: None,
            thread: None,
            result: None,
            committed_geometry: None,
            pointer: None,
            drag: None,
            scroll_remainder: [0.0; 2],
            modifiers: bmux_frontend_runtime::input::Modifiers::default(),
        },
        WindowOptions {
            decorations,
            ..WindowOptions::default()
        },
        native,
        preferences,
    )
}

fn sandbox_roots(
    context: &bmux_cli::application_execution::ValidatedExecutionContext,
) -> crate::runtime_paths::RuntimeRoots {
    let paths = &context.config_paths;
    crate::runtime_paths::RuntimeRoots {
        config_dir: paths.config_dir.clone(),
        runtime_dir: paths.runtime_dir.clone(),
        data_dir: paths.data_dir.clone(),
        state_dir: paths.state_dir.clone(),
    }
}

impl Arguments {
    fn capture_configuration(
        &self,
    ) -> Result<(
        crate::configuration::ResolvedConfiguration,
        Option<bmux_cli::application_execution::ValidatedExecutionContext>,
        Option<bmux_cli::application_execution::ValidatedSlotContext>,
    )> {
        use crate::configuration::{ConfigOptions, SourcePaths, SourcePolicy};
        use bmux_cli::application_execution::{CapturedEnvironment, ConfigurationInheritance};
        let environment = CapturedEnvironment::capture();
        let sandbox = bmux_cli::application_execution::recognize(
            &environment,
            self.execution_context.as_deref(),
        )?;
        let slot = self
            .slot
            .as_ref()
            .map(|name| {
                bmux_cli::application_execution::capture_slot(
                    name,
                    self.slots_manifest
                        .as_deref()
                        .context("slot requires explicit manifest")?,
                    &environment,
                )
            })
            .transpose()?;
        anyhow::ensure!(
            sandbox.is_none() || slot.is_none(),
            "slot selection conflicts with sandbox execution context"
        );
        if sandbox.is_some() {
            anyhow::ensure!(
                self.runtime_root.is_none()
                    && self.managed_root.is_none()
                    && self.companion.is_none(),
                "sandbox execution context conflicts with explicit runtime or companion selection"
            );
        }
        let mut options = ConfigOptions {
            policy: if self.no_bmux_config {
                SourcePolicy::BtermOnly
            } else {
                SourcePolicy::Inherit
            },
            bterm: SourcePaths {
                cli: self.config.clone(),
                ..SourcePaths::default()
            },
            bmux: SourcePaths {
                cli: self.bmux_config.clone(),
                ..SourcePaths::default()
            },
            forced_profile: self.config_profile.clone(),
            ..ConfigOptions::default()
        };
        let cwd = std::env::current_dir()?;
        options.scope = self
            .config_scope
            .as_ref()
            .map(|name| bmux_config::ConfigScopeTarget::with_cwd(name.clone(), cwd.clone()));
        let mut inputs = if let Some(context) = &sandbox {
            anyhow::ensure!(
                self.config.is_none() && self.bmux_config.is_none(),
                "sandbox configuration is selected by its descriptor; overlays require a new sandbox snapshot"
            );
            options.policy = SourcePolicy::Explicit;
            options.bterm.primary = Some(context.root.join("config/bterm/bterm.toml"));
            options.bmux.primary = Some(match &context.configuration {
                ConfigurationInheritance::Clean => context.config_paths.config_file(),
                ConfigurationInheritance::Snapshot { path, .. } => path.clone(),
            });
            let selected_environment = context
                .environment
                .0
                .iter()
                .filter_map(|(key, value)| {
                    key.clone()
                        .into_string()
                        .ok()
                        .map(|key| (key, value.clone()))
                })
                .collect();
            options.capture_selected_with_environment(
                crate::runtime_paths::BtermPaths::from_root(context.root.clone())?,
                cwd,
                selected_environment,
            )?
        } else if let Some(slot) = &slot {
            options.policy = SourcePolicy::Explicit;
            let paths = crate::runtime_paths::BtermPaths {
                config_dir: slot.config_paths.config_dir.clone(),
                cache_dir: slot.config_paths.state_dir.join("cache"),
                data_dir: slot.config_paths.data_dir.clone(),
                state_dir: slot.config_paths.state_dir.clone(),
                log_dir: slot.log_dir.clone(),
            };
            options.bterm.primary = Some(paths.config_file());
            options.bterm.state = Some(paths.state_config_file());
            options.bmux.base = slot.base_config_path.clone();
            options.bmux.primary = Some(slot.config_paths.config_file());
            options.bmux.state = Some(slot.config_paths.state_config_file());
            let selected = environment
                .0
                .iter()
                .filter_map(|(key, value)| {
                    key.clone()
                        .into_string()
                        .ok()
                        .map(|key| (key, value.clone()))
                })
                .collect();
            options.capture_selected_with_environment(paths, cwd, selected)?
        } else {
            let environment = environment
                .0
                .into_iter()
                .filter_map(|(key, value)| key.into_string().ok().map(|key| (key, value)))
                .collect();
            options.capture_with_environment(cwd, environment, self.managed_root.as_deref())?
        };
        if let Some(context) = &sandbox
            && let Some(frozen) = &context.bterm_configuration
        {
            inputs.bterm = frozen.clone();
        }
        Ok((inputs.resolve()?, sandbox, slot))
    }
}

/// The connection and its executor are one ownership unit. Dropping an
/// intermediate executor and reconnecting would lose transport tasks and reopen
/// admission against a potentially different runtime incarnation.
struct PreparedConnection<C> {
    client: C,
    runtime: tokio::runtime::Runtime,
}

struct BmuxWorker {
    preparation: Option<crate::runtime_launch::NativePreparation>,
    prepared: Option<PreparedConnection<bmux_client::BmuxClient>>,
    selection: RuntimeSelection,
    configuration: Option<crate::configuration::ResolvedConfiguration>,
    native_reload:
        std::sync::Arc<std::sync::Mutex<Option<crate::configuration::NativeConfiguration>>>,
    frozen_configuration: bool,
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
    fn take_native_configuration(&mut self) -> Option<crate::configuration::NativeConfiguration> {
        self.native_reload.lock().ok()?.take()
    }

    fn input_queue_metrics(&self) -> Option<bmux_cli::attach::QueueMetricsSnapshot> {
        self.handle
            .as_ref()
            .map(bmux_cli::attach::NativeAttachHandle::input_queue_metrics)
    }

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
        let configuration = self.configuration.clone();
        if let Some(configuration) = &configuration {
            let captured = configuration.clone();
            let native_reload = std::sync::Arc::clone(&self.native_reload);
            let last_native = std::sync::Mutex::new(captured.native.clone());
            let frozen = self.frozen_configuration
                || matches!(self.selection, RuntimeSelection::External { .. });
            host.set_config_loader(move || {
                if frozen {
                    return Ok(captured.bmux.clone());
                }
                let next = captured.reload().map_err(|error| bmux_config::ConfigError::ReadError { error: error.to_string() })?;
                if next.bmux_toml != captured.bmux_toml
                    || next.native.window != captured.native.window
                    || next.native.companion != captured.native.companion {
                    return Err(bmux_config::ConfigError::ReadError { error: "BMUX/runtime or window creation policy changed; reopen with the newly selected configuration generation".into() });
                }
                let mut previous = last_native.lock().map_err(|_| bmux_config::ConfigError::ReadError { error: "native configuration history unavailable".into() })?;
                if *previous != next.native {
                    *native_reload.lock().map_err(|_| bmux_config::ConfigError::ReadError { error: "native reload handoff unavailable".into() })? = Some(next.native.clone());
                    *previous = next.native;
                }
                Ok(next.bmux)
            });
        }
        host.set_clipboard(move |request| clipboard.clipboard_request(request));
        self.handle = Some(handle);
        let config = self
            .config
            .take()
            .context("BMUX frontend was already started")?;
        let prepared = self
            .prepared
            .take()
            .context("verified companion connection was already transferred")?;
        let mut preparation = self.preparation.take();
        if let Some(preparation) = &mut preparation {
            host.set_attach_committed(preparation.commit_callback());
        }
        let paths = self.selection.roots().config_paths();
        let target = self.target.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (admit_tx, admit_rx) = std::sync::mpsc::sync_channel(1);
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        self.result = Some(result_rx);
        self.thread = Some(
            std::thread::Builder::new()
                .name("bterm-attach".into())
                .spawn(move || {
                    let result = (|| -> Result<()> {
                        // This state is thread-local: initialize it on the same
                        // thread that polls attachment, and report failure while
                        // the window is still hidden.
                        if let Err(error) =
                            bmux_cli::attach::initialize_native_runtime_state(&config, &paths)
                        {
                            let _ = ready_tx.send(Err(format!("{error:#}")));
                            return Err(error);
                        }
                        ready_tx
                            .send(Ok(()))
                            .map_err(|_| anyhow::anyhow!("native startup was abandoned"))?;
                        // A late initialization result cannot admit attachment
                        // after the window thread's bounded wait has failed.
                        admit_rx
                            .recv_timeout(std::time::Duration::from_secs(10))
                            .context("native attachment admission was abandoned")?;
                        let PreparedConnection { client, runtime } = prepared;
                        // Never retry this operation: default-target resolution
                        // and attach may already have admitted execution effects.
                        runtime.block_on(async {
                            let attach = async {
                                bmux_cli::attach::run_native_with_client(
                                    client, &target, config, host,
                                )
                                .await?;
                                Ok(())
                            };
                            if let Some(preparation) = preparation {
                                preparation.run(attach).await
                            } else {
                                attach.await
                            }
                        })
                    })();
                    let _ = result_tx.send(result);
                    let _ = output.wake();
                })
                .context("start BMUX attachment worker")?,
        );
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .context("wait for native attachment initialization")?
            .map_err(anyhow::Error::msg)?;
        admit_tx
            .send(())
            .context("admit initialized native attachment")?;
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
    run_window_with_options(worker, WindowOptions::default())
}

fn run_window_prepared(
    event_loop: bterm_native_platform::NativeEventLoop,
    worker: impl AttachWorker,
    options: WindowOptions,
    native: crate::configuration::NativeConfiguration,
    preferences: crate::zoom_preferences::PreferenceStore,
) -> Result<()> {
    event_loop
        .run(
            options,
            Application::configured(worker, native, Some(preferences))?,
        )
        .context("run native window")
}

fn run_window_with_options(worker: impl AttachWorker, options: WindowOptions) -> Result<()> {
    bterm_native_platform::run(options, Application::new(worker)?).context("run native window")
}

fn projection_resize_needed(
    previous: Option<WindowMetrics>,
    next: WindowMetrics,
    active: bool,
) -> bool {
    !active || previous != Some(next)
}

// A wake services queued work and retries, never an exposure. Resize/resume
// request an actual redraw after invalidating their viewport as before.
fn replay_retained(
    event: &PlatformEvent,
    active: bool,
    drawable: bool,
    retained_generation: Option<u64>,
    viewport_generation: u64,
) -> bool {
    matches!(event, PlatformEvent::RedrawRequested)
        && active
        && drawable
        && retained_generation == Some(viewport_generation)
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

fn apply_window_typography(
    renderer: &mut NativeRenderer,
    mut typography: bterm_native_renderer::text::RendererTypography,
    dpi: f32,
    size: bterm_native_platform::PhysicalSize,
) -> Result<()> {
    // Reserve headroom beneath the renderer's cell/vertex budgets. Tiny valid
    // user targets must not publish millions of cells and fail every retry.
    let previous = renderer.typography().clone();
    let result = (|| -> Result<()> {
        for _ in 0..8 {
            renderer.set_typography_and_scale(typography.clone(), dpi)?;
            let cell = renderer.cell_size();
            let columns = projection_cells(size.width, cell[0])?;
            let rows = projection_cells(size.height, cell[1])?;
            let count = u64::from(columns) * u64::from(rows);
            if count <= 100_000 {
                return Ok(());
            }
            let factor = ((count as f64 / 100_000.0).sqrt() * 1.01) as f32;
            let next = (typography.font_size * factor).min(256.0_f32.min(256.0 / dpi));
            anyhow::ensure!(
                next > typography.font_size,
                "native surface exceeds bounded typography capacity"
            );
            typography.letter_spacing =
                (typography.letter_spacing * (next / typography.font_size)).min(128.0);
            typography.font_size = next;
        }
        bail!("native surface exceeds bounded typography capacity")
    })();
    if result.is_err() {
        // Restore last validated font policy; the failed candidate was never
        // published as output or interaction authority.
        renderer.set_typography_and_scale(previous, dpi)?;
    }
    result
}

fn allocation_matches_viewport(
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    viewport: PresentationViewport,
) -> bool {
    x == 0 && y == 0 && width == viewport.columns && height == viewport.rows
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
    fn wake_never_replays_but_exposure_requires_current_drawable_output() {
        let redraw = &PlatformEvent::RedrawRequested;
        let wake = &PlatformEvent::Wake;
        assert!(!replay_retained(wake, true, true, Some(4), 4));
        assert!(replay_retained(redraw, true, true, Some(4), 4));
        assert!(!replay_retained(redraw, false, true, Some(4), 4));
        assert!(!replay_retained(redraw, true, false, Some(4), 4));
        assert!(!replay_retained(redraw, true, true, None, 4));
        // Resize invalidates the old output; its requested redraw must wait for
        // queued new-viewport delivery rather than replay stale retained pixels.
        assert!(!replay_retained(redraw, true, true, Some(4), 5));
        assert!(replay_retained(redraw, true, true, Some(5), 5));
    }

    #[test]
    fn displayed_allocation_must_match_complete_native_grid() {
        let viewport = PresentationViewport {
            columns: 80,
            rows: 24,
            generation: 1,
        };
        assert!(allocation_matches_viewport(0, 0, 80, 24, viewport));
        assert!(!allocation_matches_viewport(0, 0, 1, 1, viewport));
        assert!(!allocation_matches_viewport(1, 0, 80, 24, viewport));
        assert!(!allocation_matches_viewport(0, 1, 80, 24, viewport));
        assert!(!allocation_matches_viewport(0, 0, 80, 25, viewport));
    }

    #[test]
    fn resumed_geometry_does_not_resend_the_initial_viewport_generation() {
        let metrics = WindowMetrics {
            physical_size: bterm_native_platform::PhysicalSize {
                width: 800,
                height: 600,
            },
            scale_factor: 1.0,
        };
        // Resume initializes the attachment at generation 1. Both the synthetic
        // post-resume resize and repeated compositor notifications are no-ops.
        assert!(!projection_resize_needed(Some(metrics), metrics, true));
        let mut resized = metrics;
        resized.physical_size.width = 900;
        assert!(projection_resize_needed(Some(metrics), resized, true));
        let mut scaled = metrics;
        scaled.scale_factor = 2.0;
        assert!(projection_resize_needed(Some(metrics), scaled, true));
        assert!(projection_resize_needed(None, metrics, true));
        assert!(projection_resize_needed(Some(metrics), metrics, false));
        let mut empty = metrics;
        empty.physical_size.width = 0;
        assert!(projection_resize_needed(Some(metrics), empty, true));
    }

    #[test]
    fn native_pointer_mapping_covers_css_and_keeps_hidden_distinct() {
        use bmux_attach_pipeline::pointer_shape::ResolvedPointerShape;
        use bmux_terminal_protocol::pointer_shape::PointerShape;
        use bterm_native_platform::CursorIcon;
        for shape in PointerShape::ALL {
            assert!(native_pointer_icon(ResolvedPointerShape::Shape(shape)).is_some());
        }
        assert_eq!(native_pointer_icon(ResolvedPointerShape::Hidden), None);
        assert_eq!(
            native_pointer_icon(ResolvedPointerShape::Shape(PointerShape::Grabbing)),
            Some(CursorIcon::Grabbing)
        );
        assert_eq!(
            native_pointer_icon(ResolvedPointerShape::Shape(PointerShape::Default)),
            Some(CursorIcon::Default)
        );
    }

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
            preparation: None,
            prepared: None,
            selection: RuntimeSelection::External {
                roots: crate::runtime_paths::RuntimeRoots::from_root(std::env::temp_dir()).unwrap(),
            },
            config: None,
            configuration: None,
            native_reload: Default::default(),
            frozen_configuration: false,
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
    fn initialized_window_can_show_before_first_displayed_frame() {
        let mut application = Application::new(worker_without_runtime()).unwrap();
        assert!(!application.ready_to_show());
        application.active = true;
        assert!(application.ready_to_show());
        assert!(application.last_output.is_none());
        assert!(application.retained_output.is_none());
        assert_eq!(application.semantic_generation, 0);
        application.active = false;
        assert!(!application.ready_to_show());
    }

    #[test]
    fn prepared_connection_transfers_its_original_executor_and_client() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = std::sync::Arc::new(());
        let expected = client.clone();
        let (release, released) = tokio::sync::oneshot::channel();
        // Model a transport task created during connection preparation. It must
        // still be driven after the client moves to the attachment thread.
        let transport = runtime.spawn(async move {
            released.await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            42
        });
        let prepared = PreparedConnection { client, runtime };
        std::thread::spawn(move || {
            let PreparedConnection { client, runtime } = prepared;
            assert!(std::sync::Arc::ptr_eq(&client, &expected));
            release.send(()).unwrap();
            assert_eq!(
                runtime.block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(5), transport)
                        .await
                        .unwrap()
                        .unwrap()
                }),
                42
            );
        })
        .join()
        .unwrap();
    }

    #[test]
    fn abandoned_prepared_connection_drops_client_before_executor() {
        struct ClientDropProbe {
            runtime: tokio::runtime::Handle,
        }
        impl Drop for ClientDropProbe {
            fn drop(&mut self) {
                // The connection may release runtime-owned transport resources
                // during drop; its executor must still be available then.
                self.runtime.block_on(async {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                });
            }
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = ClientDropProbe {
            runtime: runtime.handle().clone(),
        };
        drop(PreparedConnection { client, runtime });
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
