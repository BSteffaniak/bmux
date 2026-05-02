use bmux_attach_layout_protocol::{AttachInputModeState, AttachMouseProtocolState};
use bmux_plugin::{ExtensionRect, RenderDamage};
use bmux_terminal_grid::{GridLimits, TerminalGridStream, TerminalProtocolTracker};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AttachCursorState {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionRenderCacheEntry {
    pub surface_id: Uuid,
    pub surface_rect: ExtensionRect,
    pub damage: RenderDamage,
    pub revision: u64,
    pub bytes: Vec<u8>,
}

pub struct PaneRenderBuffer {
    pub terminal_grid: TerminalGridStream,
    pub protocol_tracker: TerminalProtocolTracker,
    pub prev_rows: Vec<String>,
    pub sync_update_in_progress: bool,
    pub expected_stream_start: Option<u64>,
    pub extension_render_cache: BTreeMap<(String, Uuid), ExtensionRenderCacheEntry>,
}

impl Default for PaneRenderBuffer {
    fn default() -> Self {
        Self {
            terminal_grid: TerminalGridStream::new(80, 24, GridLimits::default())
                .expect("default pane render grid dimensions are valid"),
            protocol_tracker: TerminalProtocolTracker::new(),
            prev_rows: Vec::new(),
            sync_update_in_progress: false,
            expected_stream_start: None,
            extension_render_cache: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct AttachPaneMouseProtocolHints {
    pub mode_hints: BTreeMap<Uuid, AttachMouseProtocolState>,
    pub input_mode_hints: BTreeMap<Uuid, AttachInputModeState>,
}
