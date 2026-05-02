//! Neutral structured terminal grid primitives.
//!
//! This crate intentionally contains no bmux pane/session/client/plugin domain
//! types. It models parsed terminal state as bounded styled rows with explicit
//! soft-wrap metadata so retained scrollback can be reflowed on resize without
//! replaying raw PTY bytes.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

mod delta;
mod model;
mod parser;
mod reflow;
mod snapshot;
mod style;

pub use delta::{GridDeltaApplyError, GridDeltaBatch, RowUpdateSnapshot};
pub use model::{Cell, Cursor, GridLimits, GridMode, PhysicalRow, TerminalGrid, TerminalGridError};
pub use parser::{TerminalGridStream, TerminalGridStreamDeltaError};
pub use snapshot::{
    CellRunSnapshot, CursorSnapshot, GridSnapshot, RowSnapshot, ScrollRegionSnapshot,
};
pub use style::{Color, Style, StyleId, StylePalette};
