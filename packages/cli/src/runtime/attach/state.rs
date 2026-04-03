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
    pub(crate) status_needs_redraw: bool,
    pub(crate) layout_needs_refresh: bool,
    pub(crate) pane_dirty_ids: BTreeSet<Uuid>,
    pub(crate) full_pane_redraw: bool,
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
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) w: u16,
    pub(crate) h: u16,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AttachCursorState {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) visible: bool,
}

/// Maximum time we will defer rendering a pane that is inside a DEC mode
/// 2026 synchronized update.  If the closing `\x1b[?2026l` hasn't arrived
/// after this deadline we render anyway to avoid a permanently stale pane
/// (e.g. if the inner application crashes mid-update).
pub(crate) const SYNC_UPDATE_TIMEOUT: Duration = Duration::from_millis(100);

pub struct PaneRenderBuffer {
    pub(crate) parser: vt100::Parser,
    pub(crate) last_alternate_screen: bool,
    /// Cached rendered row strings from the previous frame.  When a row's
    /// string matches the cached version we skip emitting it, avoiding
    /// unnecessary terminal I/O for unchanged content.
    pub(crate) prev_rows: Vec<String>,
    /// True while the inner application has sent `\x1b[?2026h` (begin
    /// synchronized update) but has not yet sent `\x1b[?2026l` (end).
    /// When set, the renderer defers drawing this pane's content so the
    /// user never sees a partially-updated screen.
    pub(crate) sync_update_in_progress: bool,
    /// Timestamp of the most recent `\x1b[?2026h` transition, used to
    /// enforce [`SYNC_UPDATE_TIMEOUT`].
    pub(crate) sync_update_started_at: Option<Instant>,
}

impl Default for PaneRenderBuffer {
    fn default() -> Self {
        Self {
            parser: vt100::Parser::new(24, 80, 4_096),
            last_alternate_screen: false,
            prev_rows: Vec::new(),
            sync_update_in_progress: false,
            sync_update_started_at: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachScrollbackCursor {
    pub(crate) row: usize,
    pub(crate) col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AttachScrollbackPosition {
    pub(crate) row: usize,
    pub(crate) col: usize,
}

pub struct AttachViewState {
    pub(crate) attached_id: Uuid,
    pub(crate) attached_context_id: Option<Uuid>,
    pub(crate) can_write: bool,
    pub(crate) ui_mode: AttachUiMode,
    pub(crate) scrollback_active: bool,
    pub(crate) scrollback_offset: usize,
    pub(crate) scrollback_cursor: Option<AttachScrollbackCursor>,
    pub(crate) selection_anchor: Option<AttachScrollbackPosition>,
    pub(crate) quit_confirmation_pending: bool,
    pub(crate) close_pane_confirmation_pending: Option<Uuid>,
    pub(crate) help_overlay_open: bool,
    pub(crate) help_overlay_scroll: usize,
    pub(crate) transient_status: Option<String>,
    pub(crate) transient_status_until: Option<Instant>,
    pub(crate) last_context_refresh_at: Option<Instant>,
    pub(crate) cached_tab_order: Vec<Uuid>,
    pub(crate) pane_buffers: BTreeMap<Uuid, PaneRenderBuffer>,
    pub(crate) pane_mouse_protocol_hints: BTreeMap<Uuid, AttachMouseProtocolState>,
    pub(crate) status_position: StatusPosition,
    pub(crate) cached_status_line: Option<AttachStatusLine>,
    pub(crate) cached_layout_state: Option<AttachLayoutState>,
    pub(crate) last_cursor_state: Option<AttachCursorState>,
    pub(crate) force_cursor_move_next_frame: bool,
    pub(crate) mouse: AttachMouseState,
    pub(crate) dirty: AttachDirtyFlags,

    // -- Image protocol support (feature-gated) --
    /// Per-pane image cache received from the server.
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    pub(crate) pane_images: BTreeMap<Uuid, Vec<bmux_ipc::AttachPaneImage>>,
    /// Per-pane last-seen image sequence numbers for delta queries.
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    pub(crate) image_sequences: BTreeMap<Uuid, u64>,
    /// Detected host terminal image capabilities.
    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    pub(crate) host_image_caps: bmux_image::HostImageCapabilities,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct AttachMouseState {
    pub(crate) config: MouseBehaviorConfig,
    pub(crate) last_position: Option<(u16, u16)>,
    pub(crate) last_event_at: Option<Instant>,
    pub(crate) hover_started_at: Option<Instant>,
    pub(crate) hovered_pane_id: Option<Uuid>,
    pub(crate) last_focused_pane_id: Option<Uuid>,
}

impl AttachViewState {
    pub(crate) fn new(attach_info: bmux_client::AttachOpenInfo) -> Self {
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
        }
    }

    pub(crate) fn set_transient_status(
        &mut self,
        message: impl Into<String>,
        now: Instant,
        ttl: Duration,
    ) {
        self.transient_status = Some(message.into());
        self.transient_status_until = Some(now + ttl);
        self.dirty.status_needs_redraw = true;
    }

    pub(crate) fn clear_expired_transient_status(&mut self, now: Instant) -> bool {
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

    pub(crate) fn transient_status_text(&self, now: Instant) -> Option<&str> {
        if self
            .transient_status_until
            .is_some_and(|until| now >= until)
        {
            return None;
        }
        self.transient_status.as_deref()
    }

    pub(crate) const fn exit_scrollback(&mut self) {
        self.scrollback_active = false;
        self.scrollback_offset = 0;
        self.scrollback_cursor = None;
        self.selection_anchor = None;
    }

    pub(crate) const fn selection_active(&self) -> bool {
        self.selection_anchor.is_some()
    }
}
