//! Capture and comparison use the single reusable implementation.
pub use bmux_playbook::screen::{
    CursorDeltaEvent, CursorPosition, OutputDrainResult, PaneDeltaResult, PaneOutputChunk,
    ScreenDeltaEvent, ScreenDeltaFormat, ScreenInspector, ScreenLineOp, terminal_grid_to_text,
};
