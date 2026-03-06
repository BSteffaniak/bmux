use crate::input::RuntimeAction;
use bmux_client::AttachLayoutState;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub(crate) enum AttachEventAction {
    Send(Vec<u8>),
    Runtime(RuntimeAction),
    Ui(RuntimeAction),
    Redraw,
    Detach,
    WindowModeUnboundKey,
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachUiMode {
    Normal,
    Window,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachExitReason {
    Detached,
    StreamClosed,
    Quit,
}

#[derive(Debug, Clone)]
pub(crate) struct AttachDirtyFlags {
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

#[derive(Clone, Copy)]
pub(crate) struct PaneRect {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) w: u16,
    pub(crate) h: u16,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct AttachCursorState {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) visible: bool,
}

pub(crate) struct PaneRenderBuffer {
    pub(crate) parser: vt100::Parser,
}

impl Default for PaneRenderBuffer {
    fn default() -> Self {
        Self {
            parser: vt100::Parser::new(24, 80, 4_096),
        }
    }
}

pub(crate) struct AttachViewState {
    pub(crate) attached_id: Uuid,
    pub(crate) can_write: bool,
    pub(crate) ui_mode: AttachUiMode,
    pub(crate) quit_confirmation_pending: bool,
    pub(crate) help_overlay_open: bool,
    pub(crate) help_overlay_scroll: usize,
    pub(crate) transient_status: Option<String>,
    pub(crate) transient_status_until: Option<Instant>,
    pub(crate) pane_buffers: BTreeMap<Uuid, PaneRenderBuffer>,
    pub(crate) cached_status_line: Option<String>,
    pub(crate) cached_layout_state: Option<AttachLayoutState>,
    pub(crate) last_cursor_state: Option<AttachCursorState>,
    pub(crate) dirty: AttachDirtyFlags,
}

impl AttachViewState {
    pub(crate) fn new(attach_info: bmux_client::AttachOpenInfo) -> Self {
        Self {
            attached_id: attach_info.session_id,
            can_write: attach_info.can_write,
            ui_mode: AttachUiMode::Normal,
            quit_confirmation_pending: false,
            help_overlay_open: false,
            help_overlay_scroll: 0,
            transient_status: None,
            transient_status_until: None,
            pane_buffers: BTreeMap::new(),
            cached_status_line: None,
            cached_layout_state: None,
            last_cursor_state: None,
            dirty: AttachDirtyFlags::default(),
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
}
