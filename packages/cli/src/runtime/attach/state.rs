use crate::input::RuntimeAction;
use crate::status::AttachStatusLine;
pub use bmux_attach_pipeline::{
    AttachCursorState, AttachScrollbackCursor, AttachScrollbackPosition, PaneRect, PaneRenderBuffer,
};
use bmux_client::AttachLayoutState;
use bmux_config::{MouseBehaviorConfig, StatusPosition};
use bmux_ipc::{AttachInputModeState, AttachMouseProtocolState, ContextSummary, SessionSummary};
use crossterm::event::MouseEvent;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
use uuid::Uuid;

use super::prompt_ui::AttachPromptState;

pub enum AttachEventAction {
    Send(Vec<u8>),
    Runtime(RuntimeAction),
    PluginCommand {
        plugin_id: String,
        command_name: String,
        args: Vec<String>,
    },
    Mouse(MouseEvent),
    Ui(RuntimeAction),
    Redraw,
    Detach,
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachUiMode {
    Normal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachExitReason {
    Detached,
    StreamClosed,
    Quit,
}

#[allow(clippy::struct_excessive_bools)] // Dirty flags are independent repaint/fetch toggles.
#[derive(Debug, Clone)]
pub struct AttachDirtyFlags {
    pub status_needs_redraw: bool,
    pub layout_needs_refresh: bool,
    pub overlay_needs_redraw: bool,
    pub pane_dirty_ids: BTreeSet<Uuid>,
    pub full_pane_redraw: bool,
}

impl Default for AttachDirtyFlags {
    fn default() -> Self {
        Self {
            status_needs_redraw: true,
            layout_needs_refresh: true,
            overlay_needs_redraw: false,
            pane_dirty_ids: BTreeSet::new(),
            full_pane_redraw: true,
        }
    }
}

#[allow(clippy::struct_excessive_bools)]
pub struct AttachViewState {
    pub attached_id: Uuid,
    pub attached_context_id: Option<Uuid>,
    pub can_write: bool,
    pub ui_mode: AttachUiMode,
    pub active_mode_id: String,
    pub active_mode_label: String,
    pub scrollback_active: bool,
    pub scrollback_offset: usize,
    pub scrollback_cursor: Option<AttachScrollbackCursor>,
    pub selection_anchor: Option<AttachScrollbackPosition>,
    pub help_overlay_open: bool,
    pub help_overlay_scroll: usize,
    pub prompt: AttachPromptState,
    pub transient_status: Option<String>,
    pub transient_status_until: Option<Instant>,
    pub control_catalog_revision: u64,
    pub cached_tab_order: Vec<Uuid>,
    pub cached_contexts: Vec<ContextSummary>,
    pub cached_sessions: Vec<SessionSummary>,
    pub pane_buffers: BTreeMap<Uuid, PaneRenderBuffer>,
    pub pane_mouse_protocol_hints: BTreeMap<Uuid, AttachMouseProtocolState>,
    pub pane_input_mode_hints: BTreeMap<Uuid, AttachInputModeState>,
    pub status_position: StatusPosition,
    pub cached_status_line: Option<AttachStatusLine>,
    pub cached_layout_state: Option<AttachLayoutState>,
    pub last_cursor_state: Option<AttachCursorState>,
    pub force_cursor_move_next_frame: bool,
    pub mouse: AttachMouseState,
    pub dirty: AttachDirtyFlags,

    // -- Image protocol support (feature-gated) --
    /// Per-pane image cache received from the server.
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    pub pane_images: BTreeMap<Uuid, Vec<bmux_ipc::AttachPaneImage>>,
    /// Per-pane last-seen image sequence numbers for delta queries.
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    pub image_sequences: BTreeMap<Uuid, u64>,
    /// Detected host terminal image capabilities.
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    pub host_image_caps: bmux_image::HostImageCapabilities,
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    pub kitty_host_state: bmux_image::compositor::KittyHostState,
    /// Cached image decode mode from config (read once at attach time).
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    pub image_decode_mode: bmux_image::config::ImageDecodeMode,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct AttachMouseState {
    pub config: MouseBehaviorConfig,
    pub last_position: Option<(u16, u16)>,
    pub last_event_at: Option<Instant>,
    pub hover_started_at: Option<Instant>,
    pub hovered_pane_id: Option<Uuid>,
    pub last_focused_pane_id: Option<Uuid>,
}

impl AttachViewState {
    pub fn new(attach_info: bmux_client::AttachOpenInfo) -> Self {
        Self {
            attached_id: attach_info.session_id,
            attached_context_id: attach_info.context_id,
            can_write: attach_info.can_write,
            ui_mode: AttachUiMode::Normal,
            active_mode_id: "normal".to_string(),
            active_mode_label: "NORMAL".to_string(),
            scrollback_active: false,
            scrollback_offset: 0,
            scrollback_cursor: None,
            selection_anchor: None,
            help_overlay_open: false,
            help_overlay_scroll: 0,
            prompt: AttachPromptState::default(),
            transient_status: None,
            transient_status_until: None,
            control_catalog_revision: 0,
            cached_tab_order: Vec::new(),
            cached_contexts: Vec::new(),
            cached_sessions: Vec::new(),
            pane_buffers: BTreeMap::new(),
            pane_mouse_protocol_hints: BTreeMap::new(),
            pane_input_mode_hints: BTreeMap::new(),
            status_position: StatusPosition::Bottom,
            cached_status_line: None,
            cached_layout_state: None,
            last_cursor_state: None,
            force_cursor_move_next_frame: false,
            mouse: AttachMouseState {
                config: MouseBehaviorConfig::default(),
                ..AttachMouseState::default()
            },
            dirty: AttachDirtyFlags::default(),
            #[cfg(any(
                feature = "image-sixel",
                feature = "image-kitty",
                feature = "image-iterm2"
            ))]
            pane_images: BTreeMap::new(),
            #[cfg(any(
                feature = "image-sixel",
                feature = "image-kitty",
                feature = "image-iterm2"
            ))]
            image_sequences: BTreeMap::new(),
            #[cfg(any(
                feature = "image-sixel",
                feature = "image-kitty",
                feature = "image-iterm2"
            ))]
            host_image_caps: bmux_image::HostImageCapabilities::default(),
            #[cfg(any(
                feature = "image-sixel",
                feature = "image-kitty",
                feature = "image-iterm2"
            ))]
            kitty_host_state: bmux_image::compositor::KittyHostState::default(),
            #[cfg(any(
                feature = "image-sixel",
                feature = "image-kitty",
                feature = "image-iterm2"
            ))]
            image_decode_mode: bmux_image::config::ImageDecodeMode::Passthrough,
        }
    }

    pub fn set_transient_status(
        &mut self,
        message: impl Into<String>,
        now: Instant,
        ttl: Duration,
    ) {
        self.transient_status = Some(message.into());
        self.transient_status_until = Some(now + ttl);
        self.dirty.status_needs_redraw = true;
    }

    pub fn clear_expired_transient_status(&mut self, now: Instant) -> bool {
        let Some(until) = self.transient_status_until else {
            return false;
        };
        if now < until {
            return false;
        }
        self.transient_status = None;
        self.transient_status_until = None;
        self.dirty.status_needs_redraw = true;
        true
    }

    pub fn transient_status_text(&self, now: Instant) -> Option<&str> {
        if self
            .transient_status_until
            .is_some_and(|until| now >= until)
        {
            return None;
        }
        self.transient_status.as_deref()
    }

    pub const fn exit_scrollback(&mut self) {
        self.scrollback_active = false;
        self.scrollback_offset = 0;
        self.scrollback_cursor = None;
        self.selection_anchor = None;
    }

    pub const fn selection_active(&self) -> bool {
        self.selection_anchor.is_some()
    }
}
