use crate::input::RuntimeAction;
use crate::status::AttachStatusLine;
use bmux_appearance::RuntimeAppearance;
use bmux_attach_layout_protocol::{
    AttachInputModeState, AttachMouseProtocolState, AttachScene, AttachSurface,
};
pub use bmux_attach_pipeline::{
    AttachCursorState, AttachScrollbackCursor, AttachScrollbackPosition, PaneRect,
    PaneRenderBuffer, PaneScrollbackView, PaneScrollbackViews, ScrollbackPin,
};
use bmux_attach_pipeline::{FrameDamage, RetainedCompositor, TerminalGraphicsCache};
use bmux_client::AttachLayoutState;
use bmux_config::{MouseBehaviorConfig, StatusPosition};
use bmux_control_catalog_plugin_api::control_catalog_state::{
    ContextRow, ContextSessionBinding, SessionRow,
};
use bmux_plugin::{AttachInputHook, AttachVisualProjectionUpdate, ExtensionRect};
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
    Paste(String),
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

    pub fn mark_surface_changed(&mut self, surface_id: Uuid, source: AttachDirtySource) {
        self.precise_frame_damage
            .mark_extension_surface_query(surface_id);
        self.push_event(source, AttachDirtyKind::PreciseDamage, None);
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

    pub fn mark_layout_refresh_and_status_dirty(&mut self, source: AttachDirtySource) {
        self.mark_layout_refresh(source);
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
            let _ = scene;
            damage.mark_extension_query();
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
    pub bracketed_paste_enabled: bool,
    pub ui_mode: AttachUiMode,
    pub active_mode_id: String,
    pub active_mode_label: String,
    /// Per-pane scrollback view positions. A pane is in scrollback if and only
    /// if it has an entry here, so the state cannot follow focus between panes.
    ///
    /// Scrollback history itself lives on the server (pane-runtime plugin);
    /// these are just this client's per-pane view offsets into that history.
    pub pane_scrollback: PaneScrollbackViews,
    /// Set when leaving frozen scrollback so the next post-event pass drains
    /// output from the preserved per-client server cursor even if the pane has
    /// gone quiet and no fresh output event arrives.
    pub scrollback_replay_pending: bool,
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
    pub scoped_pane_appearances: BTreeMap<Uuid, RuntimeAppearance>,
    pub terminal_graphics_cache: TerminalGraphicsCache,
    pub clipboard_sync_state: ClipboardSyncState,
    pub pane_mouse_protocol_hints: BTreeMap<Uuid, AttachMouseProtocolState>,
    pub pane_input_mode_hints: BTreeMap<Uuid, AttachInputModeState>,
    pub status_position: StatusPosition,
    pub cached_status_line: Option<AttachStatusLine>,
    pub cached_layout_state: Option<AttachLayoutState>,
    pub retained_compositor: RetainedCompositor,
    /// Opaque overlay coverage used to occlude pane terminal graphics on the
    /// previous retained frame. Content-only edits within stable bounds must
    /// not invalidate every pane extension.
    pub opaque_overlay_rects: Vec<ExtensionRect>,
    pub last_help_overlay_surface: Option<AttachSurface>,
    pub last_prompt_overlay_surface: Option<AttachSurface>,
    pub last_cursor_state: Option<AttachCursorState>,
    pub force_cursor_move_next_frame: bool,
    pub mouse: AttachMouseState,
    /// Active inline tab rename editor, when a tab label is being edited.
    pub tab_rename: Option<AttachTabRename>,
    /// Open tab context menu, when one has been summoned.
    pub tab_menu: Option<AttachTabMenu>,
    pub visual_projection_updates: Vec<AttachVisualProjectionUpdate>,
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

#[derive(Debug, Clone, Default)]
pub struct ClipboardSyncState {
    pub attempt_at: Option<Instant>,
    pub activity_at: Option<Instant>,
    pub payload_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachPointerOwner {
    Plugin,
    StatusTab,
    Resize,
    Floating,
    Selection,
}

/// Action a tab context-menu item performs when activated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachTabMenuAction {
    Rename,
    Close,
    MoveLeft,
    MoveRight,
    MoveToFirst,
    MoveToLast,
    NewWindow,
}

impl AttachTabMenuAction {
    /// Stable identifier, used by tests and playbook assertions.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::Close => "close",
            Self::MoveLeft => "move-left",
            Self::MoveRight => "move-right",
            Self::MoveToFirst => "move-to-first",
            Self::MoveToLast => "move-to-last",
            Self::NewWindow => "new-window",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Rename => "Rename",
            Self::Close => "Close",
            Self::MoveLeft => "Move left",
            Self::MoveRight => "Move right",
            Self::MoveToFirst => "Move to first",
            Self::MoveToLast => "Move to last",
            Self::NewWindow => "New window",
        }
    }
}

/// One entry in the tab context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachTabMenuItem {
    pub action: AttachTabMenuAction,
    /// Disabled entries render dimmed and cannot be activated. Position-based
    /// moves are disabled (not hidden) at the ends so the menu keeps a stable
    /// shape across tabs.
    pub enabled: bool,
}

/// Open tab context menu, anchored at the click that opened it.
#[derive(Debug, Clone)]
pub struct AttachTabMenu {
    /// Context (window) the menu acts on. Not necessarily the active window.
    pub context_id: Uuid,
    pub anchor_col: u16,
    pub anchor_row: u16,
    pub items: Vec<AttachTabMenuItem>,
    pub focused: usize,
}

impl AttachTabMenu {
    /// Build the menu for `context_id` given its position in the tab strip.
    #[must_use]
    pub fn new(
        context_id: Uuid,
        anchor_col: u16,
        anchor_row: u16,
        index: usize,
        count: usize,
    ) -> Self {
        let first = index == 0;
        let last = index + 1 >= count;
        let items = vec![
            AttachTabMenuItem {
                action: AttachTabMenuAction::Rename,
                enabled: true,
            },
            AttachTabMenuItem {
                action: AttachTabMenuAction::Close,
                enabled: true,
            },
            AttachTabMenuItem {
                action: AttachTabMenuAction::MoveLeft,
                enabled: !first,
            },
            AttachTabMenuItem {
                action: AttachTabMenuAction::MoveRight,
                enabled: !last,
            },
            AttachTabMenuItem {
                action: AttachTabMenuAction::MoveToFirst,
                enabled: !first,
            },
            AttachTabMenuItem {
                action: AttachTabMenuAction::MoveToLast,
                enabled: !last,
            },
            AttachTabMenuItem {
                action: AttachTabMenuAction::NewWindow,
                enabled: true,
            },
        ];
        let focused = items
            .iter()
            .position(|item| item.enabled)
            .unwrap_or_default();
        Self {
            context_id,
            anchor_col,
            anchor_row,
            items,
            focused,
        }
    }

    /// Move focus by `delta`, skipping disabled entries and wrapping.
    pub fn move_focus(&mut self, delta: isize) {
        let len = self.items.len();
        if len == 0 || !self.items.iter().any(|item| item.enabled) {
            return;
        }
        let mut index = self.focused;
        for _ in 0..len {
            let next = isize::try_from(index).unwrap_or(0) + delta;
            index = next.rem_euclid(isize::try_from(len).unwrap_or(1)) as usize;
            if self.items.get(index).is_some_and(|item| item.enabled) {
                self.focused = index;
                return;
            }
        }
    }

    /// Focus the first or last enabled entry.
    pub fn focus_edge(&mut self, last: bool) {
        let found = if last {
            self.items.iter().rposition(|item| item.enabled)
        } else {
            self.items.iter().position(|item| item.enabled)
        };
        if let Some(index) = found {
            self.focused = index;
        }
    }

    /// Currently focused action, when it is enabled.
    #[must_use]
    pub fn focused_action(&self) -> Option<AttachTabMenuAction> {
        self.items
            .get(self.focused)
            .filter(|item| item.enabled)
            .map(|item| item.action)
    }

    /// Rendered width, including borders and padding.
    #[must_use]
    pub fn width(&self) -> u16 {
        let widest = self
            .items
            .iter()
            .map(|item| item.action.label().chars().count())
            .max()
            .unwrap_or(0);
        u16::try_from(widest.saturating_add(4)).unwrap_or(u16::MAX)
    }

    /// Rendered height, including borders.
    #[must_use]
    pub fn height(&self) -> u16 {
        u16::try_from(self.items.len().saturating_add(2)).unwrap_or(u16::MAX)
    }
}

/// Inline tab-label editor state.
///
/// While active, the edited tab renders the raw buffer text instead of its
/// templated label, and keyboard input is routed to the buffer rather than the
/// focused pane.
#[derive(Debug, Clone)]
pub struct AttachTabRename {
    /// Context (window) being renamed.
    pub context_id: Uuid,
    /// Editable name buffer. Opens with the whole name selected so typing
    /// replaces it, while arrow keys move to an insertion point instead.
    pub buffer: bmux_text_edit::TextEditBuffer,
    /// Name to restore when the edit is cancelled.
    pub original: String,
}

impl AttachTabRename {
    #[must_use]
    pub fn new(context_id: Uuid, name: impl Into<String>) -> Self {
        let original = name.into();
        let mut buffer = bmux_text_edit::TextEditBuffer::from_text(original.clone());
        buffer.select_all();
        Self {
            context_id,
            buffer,
            original,
        }
    }

    /// Current buffer text.
    #[must_use]
    pub fn text(&self) -> &str {
        self.buffer.text()
    }

    /// Trimmed committed name, or `None` when it is blank or unchanged.
    #[must_use]
    pub fn committed_name(&self) -> Option<String> {
        let trimmed = self.buffer.text().trim();
        if trimmed.is_empty() || trimmed == self.original {
            return None;
        }
        Some(trimmed.to_string())
    }
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
    /// Context id of the status-bar tab currently under the pointer.
    pub hovered_tab_context_id: Option<Uuid>,
    /// Most recent left-button press cell and time, used to detect double clicks.
    pub last_click: Option<(u16, u16, Instant)>,
    pub last_focused_pane_id: Option<Uuid>,
    pub resize_drag: Option<AttachMouseResizeDrag>,
    pub floating_drag: Option<AttachMouseFloatingDrag>,
    pub selection_drag: Option<AttachMouseSelectionDrag>,
    pub tab_drag: Option<AttachMouseTabDrag>,
    pub input_capture: Option<AttachInputHookCapture>,
    pub input_hook_last_dispatched_at: BTreeMap<String, Instant>,
}

impl AttachMouseState {
    #[must_use]
    pub(crate) fn pointer_owner(&self) -> Option<AttachPointerOwner> {
        if self
            .input_capture
            .as_ref()
            .is_some_and(|capture| capture.pointer)
        {
            Some(AttachPointerOwner::Plugin)
        } else if self.tab_drag.is_some() {
            Some(AttachPointerOwner::StatusTab)
        } else if self.resize_drag.is_some() {
            Some(AttachPointerOwner::Resize)
        } else if self.floating_drag.is_some() {
            Some(AttachPointerOwner::Floating)
        } else if self.selection_drag.is_some() {
            Some(AttachPointerOwner::Selection)
        } else {
            None
        }
    }

    #[must_use]
    pub(crate) fn pointer_owner_count(&self) -> usize {
        usize::from(
            self.input_capture
                .as_ref()
                .is_some_and(|capture| capture.pointer),
        ) + usize::from(self.tab_drag.is_some())
            + usize::from(self.resize_drag.is_some())
            + usize::from(self.floating_drag.is_some())
            + usize::from(self.selection_drag.is_some())
    }

    #[must_use]
    pub(crate) fn has_single_pointer_owner(&self) -> bool {
        self.pointer_owner_count() <= 1
    }

    pub(crate) fn clear_plugin_pointer_capture(&mut self) {
        if let Some(capture) = self.input_capture.as_mut() {
            capture.pointer = false;
            if capture.keyboard_keys.is_empty() {
                self.input_capture = None;
            }
        }
    }

    pub(crate) fn clear_pointer_gestures(&mut self) {
        self.clear_plugin_pointer_capture();
        self.tab_drag = None;
        self.hovered_tab_context_id = None;
        // `last_click` is deliberately preserved: it is click *history* used for
        // double-click detection, not an in-flight gesture. Clearing it here
        // would break double-click, because arming a drag on the first press
        // routes through this function.
        self.resize_drag = None;
        self.floating_drag = None;
        self.selection_drag = None;
        debug_assert!(self.has_single_pointer_owner());
    }

    pub(crate) fn clear_mutation_pointer_gestures(&mut self) {
        self.resize_drag = None;
        self.floating_drag = None;
        debug_assert!(self.has_single_pointer_owner());
    }

    pub(crate) fn prepare_pointer_owner_acquisition(&mut self) {
        self.clear_pointer_gestures();
        debug_assert!(self.pointer_owner().is_none());
    }

    /// Record a left-button press and report whether it completes a
    /// double-click: the same cell pressed twice within
    /// `behavior.mouse.double_click_ms`.
    ///
    /// A detected double-click consumes the stored click so a third press
    /// starts a fresh sequence rather than chaining.
    pub(crate) fn record_click_and_detect_double(
        &mut self,
        col: u16,
        row: u16,
        now: Instant,
    ) -> bool {
        let window = Duration::from_millis(self.config.double_click_ms);
        let is_double = !window.is_zero()
            && self.last_click.is_some_and(|(last_col, last_row, at)| {
                last_col == col && last_row == row && now.saturating_duration_since(at) <= window
            });
        self.last_click = if is_double {
            None
        } else {
            Some((col, row, now))
        };
        is_double
    }

    pub(crate) fn debug_assert_single_pointer_owner(&self) {
        debug_assert!(self.has_single_pointer_owner());
    }

    pub(crate) fn normalize_pointer_owner(&mut self) -> Option<AttachPointerOwner> {
        let owner = self.pointer_owner();
        if self.has_single_pointer_owner() {
            return owner;
        }

        match owner {
            Some(AttachPointerOwner::Plugin) => {
                self.tab_drag = None;
                self.resize_drag = None;
                self.floating_drag = None;
                self.selection_drag = None;
            }
            Some(AttachPointerOwner::StatusTab) => {
                self.resize_drag = None;
                self.floating_drag = None;
                self.selection_drag = None;
            }
            Some(AttachPointerOwner::Resize) => {
                self.floating_drag = None;
                self.selection_drag = None;
            }
            Some(AttachPointerOwner::Floating) => {
                self.selection_drag = None;
            }
            Some(AttachPointerOwner::Selection) | None => {}
        }
        debug_assert!(self.has_single_pointer_owner());
        owner
    }
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
            hovered_tab_context_id: None,
            last_click: None,
            last_focused_pane_id: None,
            resize_drag: None,
            floating_drag: None,
            selection_drag: None,
            tab_drag: None,
            input_capture: None,
            input_hook_last_dispatched_at: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachInputHookCapture {
    pub hook: AttachInputHook,
    pub pointer: bool,
    pub keyboard_keys: Vec<String>,
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
    RenameWindow {
        context_id: Uuid,
        name: String,
    },
    CloseWindow {
        context_id: Uuid,
    },
    NewWindow,
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
            bracketed_paste_enabled: false,
            ui_mode: AttachUiMode::Normal,
            active_mode_id: "normal".to_string(),
            active_mode_label: "NORMAL".to_string(),
            pane_scrollback: PaneScrollbackViews::new(),
            scrollback_replay_pending: false,
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
            scoped_pane_appearances: BTreeMap::new(),
            terminal_graphics_cache: TerminalGraphicsCache::new(),
            clipboard_sync_state: ClipboardSyncState::default(),
            pane_mouse_protocol_hints: BTreeMap::new(),
            pane_input_mode_hints: BTreeMap::new(),
            status_position: StatusPosition::Bottom,
            cached_status_line: None,
            cached_layout_state: None,
            retained_compositor: RetainedCompositor::new(),
            opaque_overlay_rects: Vec::new(),
            last_help_overlay_surface: None,
            last_prompt_overlay_surface: None,
            last_cursor_state: None,
            force_cursor_move_next_frame: false,
            mouse: AttachMouseState {
                config: MouseBehaviorConfig::default(),
                ..AttachMouseState::default()
            },
            tab_rename: None,
            tab_menu: None,
            visual_projection_updates: Vec::new(),
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

    #[must_use]
    pub fn runtime_appearance_for_pane<'a>(
        &'a self,
        pane_id: &Uuid,
        fallback: &'a RuntimeAppearance,
    ) -> &'a RuntimeAppearance {
        self.scoped_pane_appearances
            .get(pane_id)
            .unwrap_or(fallback)
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

    /// Pane that keyboard scrollback actions apply to.
    #[must_use]
    pub fn focused_pane_id(&self) -> Option<Uuid> {
        Some(self.cached_layout_state.as_ref()?.focused_pane_id)
    }

    /// Scrollback view for one specific pane, if that pane is in scrollback.
    #[must_use]
    pub fn scrollback_for(&self, pane_id: Uuid) -> Option<PaneScrollbackView> {
        self.pane_scrollback.get(&pane_id).copied()
    }

    pub fn scrollback_for_mut(&mut self, pane_id: Uuid) -> Option<&mut PaneScrollbackView> {
        self.pane_scrollback.get_mut(&pane_id)
    }

    /// Whether one specific pane is in scrollback.
    #[must_use]
    pub fn scrollback_active_for(&self, pane_id: Uuid) -> bool {
        self.pane_scrollback.contains_key(&pane_id)
    }

    /// Scrollback view of the focused pane, if any.
    #[must_use]
    pub fn focused_scrollback(&self) -> Option<PaneScrollbackView> {
        self.scrollback_for(self.focused_pane_id()?)
    }

    pub fn focused_scrollback_mut(&mut self) -> Option<&mut PaneScrollbackView> {
        let pane_id = self.focused_pane_id()?;
        self.scrollback_for_mut(pane_id)
    }

    /// Whether the *focused* pane is in scrollback.
    ///
    /// This derives the UI-level "scroll mode" from the focused pane rather
    /// than storing it globally, so focusing a pane that is not scrolled back
    /// reports `false` without any explicit focus-change bookkeeping.
    #[must_use]
    pub fn scrollback_active(&self) -> bool {
        self.focused_pane_id()
            .is_some_and(|pane_id| self.scrollback_active_for(pane_id))
    }

    /// Leave scrollback for one specific pane.
    pub fn exit_scrollback_for(&mut self, pane_id: Uuid) -> bool {
        self.pane_scrollback.remove(&pane_id).is_some()
    }

    /// Leave scrollback for the focused pane only.
    pub fn exit_focused_scrollback(&mut self) -> bool {
        self.focused_pane_id()
            .is_some_and(|pane_id| self.exit_scrollback_for(pane_id))
    }

    /// Drop scrollback views for panes that no longer exist.
    pub fn retain_scrollback_panes(&mut self, active_pane_ids: &BTreeSet<Uuid>) {
        self.pane_scrollback
            .retain(|pane_id, _| active_pane_ids.contains(pane_id));
    }

    /// Whether one specific pane has an active selection.
    #[must_use]
    pub fn selection_active_for(&self, pane_id: Uuid) -> bool {
        self.scrollback_for(pane_id)
            .is_some_and(|view| view.selection_anchor.is_some())
    }

    /// Whether the focused pane has an active selection.
    #[must_use]
    pub fn selection_active(&self) -> bool {
        self.focused_scrollback()
            .is_some_and(|view| view.selection_anchor.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin::{AttachInputEndpoint, AttachInputHookFilter};

    fn test_input_hook() -> AttachInputHook {
        AttachInputHook {
            id: "test-hook".to_string(),
            owner_plugin_id: "test-plugin".to_string(),
            priority: 0,
            endpoint: AttachInputEndpoint {
                capability: "test".to_string(),
                interface_id: "test".to_string(),
                operation: "test".to_string(),
            },
            filter: AttachInputHookFilter {
                mouse_phases: Vec::new(),
                keys: Vec::new(),
                scope: "global".to_string(),
                min_interval_ms: 0,
            },
        }
    }

    fn test_tab_drag() -> AttachMouseTabDrag {
        AttachMouseTabDrag {
            source_context_id: Uuid::from_u128(2),
            started_col: 1,
            started_row: 1,
            active: false,
            drop_target: None,
        }
    }

    fn test_floating_drag() -> AttachMouseFloatingDrag {
        AttachMouseFloatingDrag {
            pane_id: Uuid::from_u128(3),
            start_x: 0,
            start_y: 0,
            width: 10,
            height: 5,
            scene_max_x: 80,
            scene_max_y: 24,
            last_x: 0,
            last_y: 0,
            start_column: 0,
            start_row: 0,
        }
    }

    fn test_resize_drag() -> AttachMouseResizeDrag {
        AttachMouseResizeDrag {
            horizontal: None,
            vertical: None,
            last_column: 0,
            last_row: 0,
            latest_column: 0,
            latest_row: 0,
            last_applied_at: Instant::now(),
        }
    }

    fn test_selection_drag() -> AttachMouseSelectionDrag {
        AttachMouseSelectionDrag {
            pane_id: Uuid::from_u128(4),
            anchor: AttachScrollbackPosition { line: 0, col: 0 },
            active: false,
        }
    }

    #[test]
    fn pointer_owner_derivation_covers_every_owner() {
        let mut mouse = AttachMouseState::default();
        assert_eq!(mouse.pointer_owner(), None);

        mouse.selection_drag = Some(test_selection_drag());
        assert_eq!(mouse.pointer_owner(), Some(AttachPointerOwner::Selection));
        mouse.selection_drag = None;

        mouse.floating_drag = Some(test_floating_drag());
        assert_eq!(mouse.pointer_owner(), Some(AttachPointerOwner::Floating));
        mouse.floating_drag = None;

        mouse.resize_drag = Some(test_resize_drag());
        assert_eq!(mouse.pointer_owner(), Some(AttachPointerOwner::Resize));
        mouse.resize_drag = None;

        mouse.tab_drag = Some(test_tab_drag());
        assert_eq!(mouse.pointer_owner(), Some(AttachPointerOwner::StatusTab));
        mouse.tab_drag = None;

        mouse.input_capture = Some(AttachInputHookCapture {
            hook: test_input_hook(),
            pointer: true,
            keyboard_keys: Vec::new(),
        });
        assert_eq!(mouse.pointer_owner(), Some(AttachPointerOwner::Plugin));
    }

    #[test]
    fn malformed_pointer_owners_are_normalized_by_precedence() {
        let mut mouse = AttachMouseState {
            tab_drag: Some(test_tab_drag()),
            resize_drag: Some(test_resize_drag()),
            floating_drag: Some(test_floating_drag()),
            selection_drag: Some(test_selection_drag()),
            ..AttachMouseState::default()
        };
        assert_eq!(mouse.pointer_owner_count(), 4);
        assert_eq!(
            mouse.normalize_pointer_owner(),
            Some(AttachPointerOwner::StatusTab)
        );
        assert!(mouse.has_single_pointer_owner());
        assert!(mouse.tab_drag.is_some());

        mouse.input_capture = Some(AttachInputHookCapture {
            hook: test_input_hook(),
            pointer: true,
            keyboard_keys: vec!["esc".to_string()],
        });
        mouse.tab_drag = Some(test_tab_drag());
        mouse.resize_drag = Some(test_resize_drag());
        mouse.floating_drag = Some(test_floating_drag());
        mouse.selection_drag = Some(test_selection_drag());
        assert_eq!(
            mouse.normalize_pointer_owner(),
            Some(AttachPointerOwner::Plugin)
        );
        assert!(mouse.has_single_pointer_owner());
        assert!(mouse.input_capture.is_some());
        assert!(mouse.tab_drag.is_none());
        assert!(mouse.resize_drag.is_none());
        assert!(mouse.floating_drag.is_none());
        assert!(mouse.selection_drag.is_none());
    }

    #[test]
    fn pointer_cancellation_preserves_keyboard_capture() {
        let mut mouse = AttachMouseState {
            tab_drag: Some(test_tab_drag()),
            resize_drag: Some(test_resize_drag()),
            floating_drag: Some(test_floating_drag()),
            selection_drag: Some(test_selection_drag()),
            input_capture: Some(AttachInputHookCapture {
                hook: test_input_hook(),
                pointer: true,
                keyboard_keys: vec!["esc".to_string()],
            }),
            ..AttachMouseState::default()
        };

        mouse.clear_pointer_gestures();

        assert_eq!(mouse.pointer_owner(), None);
        let capture = mouse.input_capture.expect("keyboard capture should remain");
        assert!(!capture.pointer);
        assert_eq!(capture.keyboard_keys, ["esc"]);
    }

    #[test]
    fn mutation_cancellation_preserves_nonmutation_owners() {
        let mut mouse = AttachMouseState {
            resize_drag: Some(test_resize_drag()),
            floating_drag: Some(test_floating_drag()),
            ..AttachMouseState::default()
        };
        mouse.clear_mutation_pointer_gestures();
        assert_eq!(mouse.pointer_owner(), None);

        mouse.selection_drag = Some(test_selection_drag());
        mouse.clear_mutation_pointer_gestures();
        assert_eq!(mouse.pointer_owner(), Some(AttachPointerOwner::Selection));
    }

    #[test]
    fn pane_scoped_appearance_overrides_fallback_deterministically() {
        let mut state = AttachViewState::new(bmux_client::AttachOpenInfo {
            session_id: Uuid::from_u128(1),
            context_id: None,
            can_write: true,
        });
        let pane_id = Uuid::from_u128(2);
        let fallback = RuntimeAppearance::default();
        let override_appearance = RuntimeAppearance {
            foreground: "#abcdef".to_string(),
            ..RuntimeAppearance::default()
        };

        assert_eq!(
            state
                .runtime_appearance_for_pane(&pane_id, &fallback)
                .foreground,
            fallback.foreground
        );

        state
            .scoped_pane_appearances
            .insert(pane_id, override_appearance.clone());
        assert_eq!(
            state
                .runtime_appearance_for_pane(&pane_id, &fallback)
                .foreground,
            override_appearance.foreground
        );

        state.scoped_pane_appearances.remove(&pane_id);
        assert_eq!(
            state
                .runtime_appearance_for_pane(&pane_id, &fallback)
                .foreground,
            fallback.foreground
        );
    }
}
