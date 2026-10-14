//! Required application adapters for playbook execution.
//!
//! These are primitive workflows, not an alternate action dispatcher. The library
//! owns deadlines, scheduling, assertions, recording and settlement. Applications
//! retain terminal acquisition, keymaps and the real attach pipeline.
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Instant;

use anyhow::Result;
use bmux_client::BmuxClient;
use bmux_config::{BmuxConfig, ConfigPaths};
use bmux_keybind::RuntimeAction;
use bmux_keyboard::KeyStroke;
use crossterm::event::Event;
use uuid::Uuid;

use crate::types::SandboxEnvMode;

pub type HostFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

pub trait KeyProcessor: Send + Sync {
    fn set_scroll_mode(&mut self, active: bool);
    fn process_stroke(&mut self, stroke: KeyStroke) -> Vec<RuntimeAction>;
    fn flush(&mut self) -> Vec<RuntimeAction>;
}

/// An acquired terminal. Dropping it must restore the application terminal.
pub trait VisualTerminal: std::fmt::Debug + Send {
    fn suspend_for_line_input(&mut self) -> Result<()>;
    fn resume_after_line_input(&mut self) -> Result<()>;
    fn size(&self) -> Result<(u16, u16)>;
    fn poll_event(&self) -> Result<Option<Event>>;
    fn paint_lines(&mut self, lines: &[String]) -> Result<()>;
}

/// Acknowledged input is paired with the committed attach target, not merely
/// enqueued. An unresolved delivery must fence subsequent input.
pub trait RealAttach: Send + Sync {
    fn send_event(&self, event: Event, deadline: Instant) -> HostFuture<'_, Uuid>;
    fn ensure_input_resolved(&self) -> Result<()>;
    fn wait_until_ready(&self) -> HostFuture<'_, ()>;
    fn resize(&self, cols: u16, rows: u16) -> Result<()>;
    fn attached_session_id(&self) -> Option<Uuid>;
    fn rendered_text(&self, rows: u16) -> Result<String>;
    fn drain_output(&self) -> Vec<Vec<u8>>;
    fn output_byte_count(&self) -> usize;
    fn detach(self: Box<Self>) -> HostFuture<'static, ()>;
}

/// Primitive operations on the application's existing simulation pipeline.
/// Assertions and dispatch remain in the executor.
pub trait AttachSimulation: Send {
    fn seed_pane_lines(&mut self, lines: &[&str], row: u16, col: u16);
    fn seed_vertical_split_panes(&mut self);
    fn seed_floating_pane_layout(&mut self);
    fn render(&mut self);
    fn resize_viewport(&mut self, cols: u16, rows: u16);
    fn rendered(&self) -> &str;
    fn send_attach_chord(&mut self, chord: &str) -> Result<Vec<String>>;
    fn effect_operations(&self) -> Vec<&'static str>;
    fn locate_text(&self, text: &str) -> Option<LocatedText>;
    fn mouse_event(&mut self, event: MouseEvent) -> Result<()>;
    fn help_overlay_open(&self) -> bool;
    fn help_overlay_scroll(&self) -> usize;
    fn prompt_active(&self) -> bool;
    fn selected_text(&mut self) -> Option<String>;
    fn scrollback_active(&self) -> bool;
    fn selection_active(&self) -> bool;
    fn scrollback_cursor(&self) -> Option<(usize, usize)>;
}

#[derive(Clone, Copy, Debug)]
pub struct LocatedText {
    pub start_col: usize,
    pub end_col: usize,
    pub center_col: usize,
    pub row: usize,
}

#[derive(Clone, Copy, Debug)]
pub enum MousePhase {
    Down,
    Up,
    Drag,
    Move,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}
#[derive(Clone, Copy, Debug)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}
#[derive(Clone, Copy, Debug)]
pub struct MouseEvent {
    pub phase: MousePhase,
    pub button: Option<MouseButton>,
    pub col: u16,
    pub row: u16,
}

pub struct SandboxManifestUpdate<'a> {
    pub root: &'a Path,
    pub paths: &'a ConfigPaths,
    pub executable: &'a Path,
    pub command: &'a [String],
    pub env_mode: SandboxEnvMode,
    pub status: &'a str,
    pub exit_code: Option<i32>,
    pub kept: bool,
}

/// Required, caller-owned execution infrastructure. There is intentionally no
/// default adapter: a non-CLI consumer must supply real capabilities explicitly.
pub trait HostAdapter: std::fmt::Debug + Send + Sync {
    fn executable(&self) -> Result<PathBuf>;
    fn pane_transport(&self) -> std::sync::Arc<dyn crate::transport::PaneTransport>;
    fn parse_key_chord(&self, chord: &str) -> Result<Vec<KeyStroke>>;
    fn key_processor(&self, config: &BmuxConfig) -> Box<dyn KeyProcessor>;
    fn terminal_is_interactive(&self) -> bool;
    fn enter_visual_terminal(&self) -> Result<Box<dyn VisualTerminal>>;
    fn read_line(&self) -> Result<String>;
    fn start_real_attach<'a>(
        &'a self,
        client: BmuxClient,
        session_id: Uuid,
        config: Option<BmuxConfig>,
        paths: Option<ConfigPaths>,
        viewport: (u16, u16),
    ) -> HostFuture<'a, Box<dyn RealAttach>>;
    fn simulation(&self, cols: u16, rows: u16) -> Result<Box<dyn AttachSimulation>>;
    fn write_sandbox_manifest(&self, update: SandboxManifestUpdate<'_>) -> Result<()>;
    fn register_sandbox(&self, root: &Path) -> Result<()>;
    fn write_sandbox_lock(&self, root: &Path, pid: u32) -> Result<()>;
    fn clear_sandbox_lock(&self, root: &Path) -> Result<()>;
}

/// Acquired application attach pipeline, consumed by execution-domain helpers.
pub struct RealAttachRuntime(pub Box<dyn RealAttach>);
impl RealAttachRuntime {
    pub(crate) async fn send_paste(&self, text: &str, deadline: Instant) -> Result<()> {
        self.wait_until_ready().await?;
        self.send_event(Event::Paste(text.to_owned()), deadline)
            .await?;
        Ok(())
    }
    pub(crate) async fn send_chord(
        &self,
        host: &dyn HostAdapter,
        chord: &str,
        deadline: Instant,
    ) -> Result<Uuid> {
        self.wait_until_ready().await?;
        let strokes = host.parse_key_chord(chord)?;
        let mut target = None;
        for stroke in strokes {
            target = Some(
                self.send_event(crate::engine::crossterm_event_from_stroke(stroke), deadline)
                    .await?,
            );
        }
        target.ok_or_else(|| anyhow::anyhow!("real attachment chord delivered no events"))
    }
    pub(crate) fn ensure_input_resolved(&self) -> Result<()> {
        self.0.ensure_input_resolved()
    }
    pub(crate) async fn wait_until_ready(&self) -> Result<()> {
        self.0.wait_until_ready().await
    }
    pub(crate) async fn send_event(&self, event: Event, deadline: Instant) -> Result<Uuid> {
        self.0.send_event(event, deadline).await
    }
    pub(crate) fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.0.resize(cols, rows)
    }
    pub(crate) fn record_pending_output(
        &self,
        track: &mut Option<crate::display_track::PlaybookDisplayTrackWriter>,
    ) {
        if let Some(track) = track.as_mut() {
            for bytes in self.0.drain_output() {
                if let Err(error) = track.record_frame_bytes(&bytes) {
                    tracing::warn!(%error, "failed recording real-attach playbook output");
                }
            }
        }
    }
    pub(crate) async fn detach(self) {
        if let Err(error) = self.0.detach().await {
            tracing::warn!(%error, "failed detaching real-attach playbook runtime");
        }
    }
}
