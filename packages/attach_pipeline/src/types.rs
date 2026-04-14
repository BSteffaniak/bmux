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

pub struct PaneRenderBuffer {
    pub parser: vt100::Parser,
    pub last_alternate_screen: bool,
    pub prev_rows: Vec<String>,
    pub sync_update_in_progress: bool,
    pub expected_stream_start: Option<u64>,
}

impl Default for PaneRenderBuffer {
    fn default() -> Self {
        Self {
            parser: vt100::Parser::new(24, 80, 4_096),
            last_alternate_screen: false,
            prev_rows: Vec::new(),
            sync_update_in_progress: false,
            expected_stream_start: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct AttachPaneMouseProtocolHints {
    pub mode_hints: BTreeMap<Uuid, bmux_ipc::AttachMouseProtocolState>,
    pub input_mode_hints: BTreeMap<Uuid, bmux_ipc::AttachInputModeState>,
}
