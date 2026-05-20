#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Native terminal UI primitives for BMUX.
//!
//! This crate is intentionally domain-agnostic. It owns reusable terminal UI
//! foundations such as geometry, style, styled text, render buffers, and layout
//! helpers. Product behavior belongs in application crates and plugins.

pub mod ansi;
pub mod buffer;
pub mod frame;
pub mod geometry;
pub mod layout;
pub mod style;
pub mod text;
pub mod widget;
pub mod widgets;

pub use ansi::{AnsiFrameDiffStats, write_ansi_frame, write_ansi_frame_diff};
pub use buffer::{Buffer, Cell};
pub use frame::{Cursor, Frame};
pub use geometry::{Insets, Point, Rect, Size};
pub use layout::{Breakpoint, Constraint, Direction, Layout, Split, split};
pub use style::{Color, Modifier, Style};
pub use text::{Line, Span, Text};
pub use widget::{StatefulWidget, Widget};
pub use widgets::{
    Alignment, Border, BorderSet, List, ListItem, ListKeyHandler, ListKeyOutcome, ListPicker,
    ListPickerAreas, ListState, Modal, Panel, TextBlock, TextInput, TextInputEnterBehavior,
    TextInputKeyHandler, TextInputKeyOutcome, TextInputProjection, TextWrap,
};

/// Common imports for building BMUX TUI surfaces.
pub mod prelude {
    pub use crate::ansi::{AnsiFrameDiffStats, write_ansi_frame, write_ansi_frame_diff};
    pub use crate::buffer::{Buffer, Cell};
    pub use crate::frame::{Cursor, Frame};
    pub use crate::geometry::{Insets, Point, Rect, Size};
    pub use crate::layout::{Breakpoint, Constraint, Direction, Layout, Split, split};
    pub use crate::style::{Color, Modifier, Style};
    pub use crate::text::{Line, Span, Text};
    pub use crate::widget::{StatefulWidget, Widget};
    pub use crate::widgets::{
        Alignment, Border, BorderSet, List, ListItem, ListKeyHandler, ListKeyOutcome, ListPicker,
        ListPickerAreas, ListState, Modal, Panel, TextBlock, TextInput, TextInputEnterBehavior,
        TextInputKeyHandler, TextInputKeyOutcome, TextInputProjection, TextWrap,
    };
}
