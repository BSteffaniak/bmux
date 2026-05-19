#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Native terminal UI primitives for BMUX.
//!
//! This crate is intentionally domain-agnostic. It owns reusable terminal UI
//! foundations such as geometry, style, styled text, render buffers, and layout
//! helpers. Product behavior belongs in application crates and plugins.

pub mod buffer;
pub mod frame;
pub mod geometry;
pub mod layout;
pub mod style;
pub mod text;
pub mod widget;
pub mod widgets;

pub use buffer::{Buffer, Cell};
pub use frame::{Cursor, Frame};
pub use geometry::{Insets, Point, Rect, Size};
pub use layout::{Breakpoint, Constraint, Direction, Layout, Split, split};
pub use style::{Color, Modifier, Style};
pub use text::{Line, Span, Text};
pub use widget::{StatefulWidget, Widget};
pub use widgets::{
    Alignment, Border, BorderSet, Panel, TextBlock, TextInput, TextInputProjection, TextWrap,
};
