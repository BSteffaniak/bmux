//! Neutral structured terminal grid primitives.
//!
//! This crate intentionally contains no bmux pane/session/client/plugin domain
//! types. It models parsed terminal state as bounded styled rows with explicit
//! soft-wrap metadata so retained scrollback can be reflowed on resize without
//! replaying raw PTY bytes.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

mod charset;
pub use charset::CharacterState;
mod filter;
pub use filter::{FilterInterest, SequenceFilter, SequenceFilters};
mod frozen;
pub use frozen::FrozenContentRows;
mod model;
mod parser;
mod reflow;
mod render;
mod snapshot;
mod style;
mod vt_parser;

#[cfg(test)]
mod vt_parser_tests;

#[cfg(test)]
mod pointer_shape_tests;

#[cfg(test)]
mod state_regression_tests;

#[cfg(test)]
mod rendition_regression_tests;

#[cfg(test)]
mod frame_tests;
#[cfg(test)]
mod stream_compat_tests;

pub use model::{
    Cell, ContentAnchor, ContentBudget, ContentCursor, ContentPosition, ContentProjection,
    ContentRowSource, ContentRows, ContentSelectionRow, Cursor, FrameApplied, FrameApplyError,
    FrameSummary, GridLimits, GridMode, GridRowWindow, HistoryLineAssembly, HistorySlice,
    HistorySliceEnd, HistorySliceError, MouseProtocolEncoding, MouseProtocolMode, PhysicalRow,
    ProjectedRows, ProtocolState, ReplicaCursor, ScreenRowSource, ScreenRows, ScreenSelectionRow,
    SelectionCell, TerminalGrid, TerminalGridError, ViewportScroll, physical_row_is_blank,
};
pub use parser::{
    ChunkMark, HeaderValues, IgnoreSequences, MAX_OBSERVED_DCS_BYTES, MAX_OBSERVED_OSC_BYTES,
    ProtocolProcessOutcome, SequenceHeader, SequenceObserver, StreamedString, TerminalGridStream,
    TerminalProtocolTracker,
};
pub use render::{
    content_text, full_screen_repaint_bytes, row_text, visible_text, visible_text_lines,
    visible_text_trimmed,
};
pub use snapshot::{
    CellRunSnapshot, CursorSnapshot, GridSnapshot, RowSnapshot, ScrollRegionSnapshot,
    TerminalStateSnapshot,
};
pub use style::{Color, Style, StyleId, StylePalette};
