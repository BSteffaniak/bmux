use crate::input::RuntimeAction;
use crate::status::AttachStatusLine;
use bmux_attach_layout_protocol::{
    AttachInputModeState, AttachMouseProtocolState, AttachScene, AttachSurface,
};
pub use bmux_attach_pipeline::{
    AttachCursorState, AttachScrollbackCursor, AttachScrollbackPosition, PaneRect, PaneRenderBuffer,
};
use bmux_attach_pipeline::{FrameDamage, RetainedCompositor};
use bmux_client::AttachLayoutState;
use bmux_config::{MouseBehaviorConfig, StatusPosition};
use bmux_control_catalog_plugin_api::control_catalog_state::{
    ContextRow, ContextSessionBinding, SessionRow,
};
use bmux_windows_plugin_api::windows_commands::PaneResizeDirection;
use bmux_windows_plugin_api::windows_list::WindowListSnapshot;
use crossterm::event::MouseEvent;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

use super::prompt_ui::AttachPromptState;

pub enum AttachEventAction {
    Send(Vec<u8>),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachDirtySource {
    PaneOutput,
    LayoutChanged,
    FocusChanged,
    SceneChanged,
    StatusChanged,
    PromptOverlay,
    HelpOverlay,
    AppearanceChanged,
    ManualRedraw,
    SnapshotHydration,
    AlternateScreenTransition,
    PluginCommand,
    UserAction,
    ActionDispatch,
    Mouse,
    Scrollback,
    Selection,
    ProfileChanged,
    ControlCatalogChanged,
    PaneLifecycle,
    FollowTargetChanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachDirtyKind {
    Pane,
    Status,
    Overlay,
    FullFrame,
    Extension,
    PreciseDamage,
    LayoutRefresh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachDirtyEvent {
    pub source: AttachDirtySource,
    pub kind: AttachDirtyKind,
    pub pane_id: Option<Uuid>,
}

#[allow(clippy::struct_excessive_bools)] // Dirty flags are independent repaint/fetch toggles.
#[derive(Debug, Clone)]
pub struct AttachDirtyFlags {
    pub status_needs_redraw: bool,
    pub layout_needs_refresh: bool,
    pub overlay_needs_redraw: bool,
    pub pane_dirty_ids: BTreeSet<Uuid>,
    pub full_pane_redraw: bool,
    /// Generic surface/plugin-render damage. This asks render
    /// extensions to repaint their damaged regions without invalidating
    /// pane content row caches.
    pub extension_needs_redraw: bool,
    /// Precise frame damage computed by scene/layout reconciliation.
    pub precise_frame_damage: FrameDamage,
    /// Low-cardinality reasons that accumulated before the next rendered frame.
    pub dirty_events: Vec<AttachDirtyEvent>,
}

impl Default for AttachDirtyFlags {
    fn default() -> Self {
        Self {
            status_needs_redraw: true,
            layout_needs_refresh: true,
            overlay_needs_redraw: false,
            pane_dirty_ids: BTreeSet::new(),
            full_pane_redraw: true,
            extension_needs_redraw: true,
            precise_frame_damage: FrameDamage::default(),
            dirty_events: Vec::new(),
        }
    }
}

impl AttachDirtyFlags {
    fn push_event(
        &mut self,
        source: AttachDirtySource,
        kind: AttachDirtyKind,
        pane_id: Option<Uuid>,
    ) {
        self.dirty_events.push(AttachDirtyEvent {
            source,
            kind,
            pane_id,
        });
    }

    pub fn mark_pane_dirty(&mut self, pane_id: Uuid, source: AttachDirtySource) {
        self.pane_dirty_ids.insert(pane_id);
        self.push_event(source, AttachDirtyKind::Pane, Some(pane_id));
    }

    pub fn mark_status_dirty(&mut self, source: AttachDirtySource) {
        self.status_needs_redraw = true;
        self.push_event(source, AttachDirtyKind::Status, None);
    }

    pub fn mark_overlay_dirty(&mut self, source: AttachDirtySource) {
        self.overlay_needs_redraw = true;
        self.push_event(source, AttachDirtyKind::Overlay, None);
    }

    pub fn mark_full_frame(&mut self, source: AttachDirtySource) {
        self.full_pane_redraw = true;
        self.push_event(source, AttachDirtyKind::FullFrame, None);
    }

    pub fn mark_extension_dirty(&mut self, source: AttachDirtySource) {
        self.extension_needs_redraw = true;
        self.push_event(source, AttachDirtyKind::Extension, None);
    }

    pub fn mark_layout_refresh(&mut self, source: AttachDirtySource) {
        self.layout_needs_refresh = true;
        self.push_event(source, AttachDirtyKind::LayoutRefresh, None);
    }

    pub fn mark_layout_frame_dirty(&mut self, source: AttachDirtySource) {
        self.mark_layout_refresh(source);
        self.mark_full_frame(source);
    }

    pub fn mark_layout_frame_and_status_dirty(&mut self, source: AttachDirtySource) {
        self.mark_layout_frame_dirty(source);
        self.mark_status_dirty(source);
    }

    pub fn merge_precise_damage(&mut self, damage: &FrameDamage, source: AttachDirtySource) {
        if damage.is_empty() {
            return;
        }
        self.precise_frame_damage.merge_from(damage);
        self.push_event(source, AttachDirtyKind::PreciseDamage, None);
    }

    #[must_use]
    pub fn dirty_events(&self) -> &[AttachDirtyEvent] {
        &self.dirty_events
    }

    #[must_use]
    pub fn frame_damage(&self, scene: &AttachScene) -> FrameDamage {
        let mut damage = if self.full_pane_redraw {
            FrameDamage::full_frame()
        } else {
            FrameDamage::default()
        };
        damage.merge_from(&self.precise_frame_damage);
        for pane_id in &self.pane_dirty_ids {
            damage.mark_content_surface(*pane_id);
        }
        if self.extension_needs_redraw {
            for surface in &scene.surfaces {
                damage.mark_extension_surface(surface.id);
            }
        }
        if self.status_needs_redraw {
            damage.mark_status();
        }
        if self.overlay_needs_redraw {
            damage.mark_overlay();
        }
        damage
    }

    #[must_use]
    pub fn needs_render(&self) -> bool {
        self.status_needs_redraw
            || self.full_pane_redraw
            || self.extension_needs_redraw
            || self.overlay_needs_redraw
            || !self.precise_frame_damage.is_empty()
            || !self.pane_dirty_ids.is_empty()
    }

    pub fn clear_frame_damage(&mut self) {
        self.full_pane_redraw = false;
        self.extension_needs_redraw = false;
        self.overlay_needs_redraw = false;
        self.precise_frame_damage = FrameDamage::default();
        self.pane_dirty_ids.clear();
        self.dirty_events.clear();
    }
}

#[allow(clippy::struct_excessive_bools)]
pub struct AttachViewState {
    pub self_client_id: Option<Uuid>,
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
    /// Latest ordered window list received from the windows-plugin
    /// `windows-list` state channel. `None` until the first snapshot
    /// arrives (or when the plugin is absent, in which case the tab
    /// bar falls back to rendering `cached_contexts` in raw server
    /// order). Updated by the attach loop whenever the plugin
    /// publishes a new value via `publish_window_list_snapshot`.
    pub cached_window_list: Option<Arc<WindowListSnapshot>>,
    pub cached_contexts: Vec<ContextRow>,
    pub cached_sessions: Vec<SessionRow>,
    pub cached_context_session_bindings: Vec<ContextSessionBinding>,
    pub pane_buffers: BTreeMap<Uuid, PaneRenderBuffer>,
    pub pane_mouse_protocol_hints: BTreeMap<Uuid, AttachMouseProtocolState>,
    pub pane_input_mode_hints: BTreeMap<Uuid, AttachInputModeState>,
    pub status_position: StatusPosition,
    pub cached_status_line: Option<AttachStatusLine>,
    pub cached_layout_state: Option<AttachLayoutState>,
    pub retained_compositor: RetainedCompositor,
    pub last_help_overlay_surface: Option<AttachSurface>,
    pub last_prompt_overlay_surface: Option<AttachSurface>,
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
    pub pane_images: BTreeMap<Uuid, Vec<bmux_attach_image_protocol::AttachPaneImage>>,
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

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AttachMouseState {
    pub config: MouseBehaviorConfig,
    pub tab_drag_enabled: bool,
    pub last_position: Option<(u16, u16)>,
    pub last_event_at: Option<Instant>,
    pub hover_started_at: Option<Instant>,
    pub hovered_pane_id: Option<Uuid>,
    pub last_focused_pane_id: Option<Uuid>,
    pub resize_drag: Option<AttachMouseResizeDrag>,
    pub floating_drag: Option<AttachMouseFloatingDrag>,
    pub selection_drag: Option<AttachMouseSelectionDrag>,
    pub tab_drag: Option<AttachMouseTabDrag>,
}

impl Default for AttachMouseState {
    fn default() -> Self {
        Self {
            config: MouseBehaviorConfig::default(),
            tab_drag_enabled: true,
            last_position: None,
            last_event_at: None,
            hover_started_at: None,
            hovered_pane_id: None,
            last_focused_pane_id: None,
            resize_drag: None,
            floating_drag: None,
            selection_drag: None,
            tab_drag: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachMouseTabDrag {
    pub source_context_id: Uuid,
    pub started_col: u16,
    pub started_row: u16,
    pub active: bool,
    pub drop_target: Option<AttachTabDropTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachTabDropTarget {
    pub context_id: Uuid,
    pub placement: AttachTabDropPlacement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachTabDropPlacement {
    Before,
    After,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachUiReduction {
    pub consumed: bool,
    pub effects: Vec<AttachUiEffect>,
}

impl AttachUiReduction {
    pub const fn ignored() -> Self {
        Self {
            consumed: false,
            effects: Vec::new(),
        }
    }

    pub const fn consumed() -> Self {
        Self {
            consumed: true,
            effects: Vec::new(),
        }
    }

    pub fn with_effect(effect: AttachUiEffect) -> Self {
        Self {
            consumed: true,
            effects: vec![effect],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachUiEffect {
    SwitchWindow {
        target_context_id: Uuid,
    },
    MoveWindow {
        source_context_id: Uuid,
        target_context_id: Uuid,
        placement: AttachTabDropPlacement,
    },
    ResizePane {
        pane_id: Uuid,
        direction: PaneResizeDirection,
        cells: u16,
    },
    FocusPane {
        pane_id: Uuid,
    },
    MoveFloatingPane {
        pane_id: Uuid,
        x: u16,
        y: u16,
    },
    ShowTransientStatus {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachMouseFloatingDrag {
    pub pane_id: Uuid,
    pub start_x: u16,
    pub start_y: u16,
    pub width: u16,
    pub height: u16,
    pub scene_max_x: u16,
    pub scene_max_y: u16,
    pub last_x: u16,
    pub last_y: u16,
    pub start_column: u16,
    pub start_row: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachMouseSelectionDrag {
    pub pane_id: Uuid,
    pub anchor: AttachScrollbackPosition,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachMouseResizeDrag {
    pub horizontal: Option<AttachMouseResizeAxisDrag>,
    pub vertical: Option<AttachMouseResizeAxisDrag>,
    pub last_column: u16,
    pub last_row: u16,
    pub latest_column: u16,
    pub latest_row: u16,
    pub last_applied_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachMouseResizeAxisDrag {
    pub positive_target_pane_id: Uuid,
    pub positive_direction: PaneResizeDirection,
    pub negative_target_pane_id: Uuid,
    pub negative_direction: PaneResizeDirection,
}

impl AttachViewState {
    pub fn new(attach_info: bmux_client::AttachOpenInfo) -> Self {
        Self {
            self_client_id: None,
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
            cached_window_list: None,
            cached_contexts: Vec::new(),
            cached_sessions: Vec::new(),
            cached_context_session_bindings: Vec::new(),
            pane_buffers: BTreeMap::new(),
            pane_mouse_protocol_hints: BTreeMap::new(),
            pane_input_mode_hints: BTreeMap::new(),
            status_position: StatusPosition::Bottom,
            cached_status_line: None,
            cached_layout_state: None,
            retained_compositor: RetainedCompositor::new(),
            last_help_overlay_surface: None,
            last_prompt_overlay_surface: None,
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
        self.dirty
            .mark_status_dirty(AttachDirtySource::StatusChanged);
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
        self.dirty
            .mark_status_dirty(AttachDirtySource::StatusChanged);
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
