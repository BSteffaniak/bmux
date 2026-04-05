use crate::input::RuntimeAction;
use crate::status::AttachStatusLine;
use bmux_client::AttachLayoutState;
use bmux_config::{MouseBehaviorConfig, StatusPosition};
use bmux_ipc::AttachMouseProtocolState;
use crossterm::event::MouseEvent;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
use uuid::Uuid;

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

#[derive(Debug, Clone)]
pub struct AttachDirtyFlags {
    pub status_needs_redraw: bool,
    pub layout_needs_refresh: bool,
    pub pane_dirty_ids: BTreeSet<Uuid>,
    pub full_pane_redraw: bool,
}

impl Default for AttachDirtyFlags {
    fn default() -> Self {
        Self {
            status_needs_redraw: true,
            layout_needs_refresh: true,
            pane_dirty_ids: BTreeSet::new(),
            full_pane_redraw: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AttachCursorState {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
}

pub struct PaneRenderBuffer {
    pub parser: vt100::Parser,
    pub last_alternate_screen: bool,
    /// Cached rendered row strings from the previous frame.  When a row's
    /// string matches the cached version we skip emitting it, avoiding
    /// unnecessary terminal I/O for unchanged content.
    pub prev_rows: Vec<String>,
    /// True while the inner application is inside a DEC mode 2026
    /// synchronized update.  Populated from the server's per-pane
    /// `sync_update_active` flag (tracked by the PTY reader's byte-by-
    /// byte CSI parser, so no cross-chunk splitting issues).  When set,
    /// the renderer defers drawing this pane's content so the user never
    /// sees a partially-updated screen.
    pub sync_update_in_progress: bool,
}

impl Default for PaneRenderBuffer {
    fn default() -> Self {
        Self {
            parser: vt100::Parser::new(24, 80, 4_096),
            last_alternate_screen: false,
            prev_rows: Vec::new(),
            sync_update_in_progress: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachScrollbackCursor {
    pub row: usize,
    pub col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AttachScrollbackPosition {
    pub row: usize,
    pub col: usize,
}

pub struct AttachViewState {
    pub attached_id: Uuid,
    pub attached_context_id: Option<Uuid>,
    pub can_write: bool,
    pub ui_mode: AttachUiMode,
    pub scrollback_active: bool,
    pub scrollback_offset: usize,
    pub scrollback_cursor: Option<AttachScrollbackCursor>,
    pub selection_anchor: Option<AttachScrollbackPosition>,
    pub quit_confirmation_pending: bool,
    pub close_pane_confirmation_pending: Option<Uuid>,
    pub help_overlay_open: bool,
    pub help_overlay_scroll: usize,
    pub transient_status: Option<String>,
    pub transient_status_until: Option<Instant>,
    pub last_context_refresh_at: Option<Instant>,
    pub cached_tab_order: Vec<Uuid>,
    pub pane_buffers: BTreeMap<Uuid, PaneRenderBuffer>,
    pub pane_mouse_protocol_hints: BTreeMap<Uuid, AttachMouseProtocolState>,
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
            scrollback_active: false,
            scrollback_offset: 0,
            scrollback_cursor: None,
            selection_anchor: None,
            quit_confirmation_pending: false,
            close_pane_confirmation_pending: None,
            help_overlay_open: false,
            help_overlay_scroll: 0,
            transient_status: None,
            transient_status_until: None,
            last_context_refresh_at: None,
            cached_tab_order: Vec::new(),
            pane_buffers: BTreeMap::new(),
            pane_mouse_protocol_hints: BTreeMap::new(),
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
