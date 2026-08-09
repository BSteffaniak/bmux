#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Native terminal UI primitives for BMUX.
//!
//! This crate is intentionally domain-agnostic. It owns reusable terminal UI
//! foundations such as geometry, style, styled text, render buffers, and layout
//! helpers. Product behavior belongs in application crates and plugins.

pub mod ansi;
pub mod blocks;
pub mod buffer;
pub mod capabilities;
pub mod chrome;
pub mod clear;
#[cfg(feature = "crossterm")]
pub mod crossterm;
pub mod damage;
pub mod dialog;
#[cfg(feature = "diff")]
pub mod diff;
pub mod event;
pub mod focus;
pub mod frame;
pub mod geometry;
pub mod history;
pub mod hit;
pub mod image;
pub mod image_scene;
pub mod input;
pub mod layout;
pub mod list;
pub mod overlay;
pub mod palette;
pub mod picker;
pub mod retained_list;
pub mod retained_sectioned_list;
pub mod style;
pub mod terminal;
pub mod text;
pub mod text_block;
pub mod text_width;
pub mod viewport;
pub mod widget;

/// Common imports for building BMUX TUI surfaces.
pub mod prelude {
    pub use crate::ansi::{
        AnsiFrameDiffStats, ansi_to_lines, write_ansi_frame, write_ansi_frame_diff,
    };
    pub use crate::blocks::{ProgressBlock, StatusBlock, StatusLevel, ToolBlock};
    pub use crate::buffer::{Buffer, Cell};
    pub use crate::capabilities::{TerminalBackground, TerminalCapabilities, TerminalColorDepth};
    pub use crate::chrome::{
        Border, BorderSet, BorderSides, Modal, Panel, PanelTitle, TitlePosition,
    };
    pub use crate::clear::Clear;
    #[cfg(feature = "crossterm")]
    pub use crate::crossterm::{
        CrosstermTerminalGuard, event_from_crossterm, key_from_crossterm, mouse_from_crossterm,
        poll_event, read_event,
    };
    pub use crate::damage::{Damage, DamagePolicy};
    pub use crate::dialog::{Button, Dialog, DialogAction, DialogState};
    #[cfg(feature = "diff")]
    pub use crate::diff::{
        DiffChangedRange, DiffFileList, DiffFileListKeyHandler, DiffFileListKeyOutcome,
        DiffFileListState, DiffFileSummary, DiffInlineSpan, DiffLine, DiffLineKind, DiffView,
        DiffViewMode, DiffViewState, DiffViewStyles,
    };
    pub use crate::event::{
        Event, EventHandler, EventOutcome, FocusEvent, MouseButton, MouseEvent, MouseEventKind,
        MouseModifiers,
    };
    pub use crate::focus::{FocusId, FocusKeyOutcome, FocusTrap};
    pub use crate::frame::{Cursor, Frame};
    pub use crate::geometry::{Insets, Point, Rect, Size};
    pub use crate::history::{TextInputHistory, TextInputHistoryDirection, TextInputHistoryState};
    pub use crate::hit::{Hit, HitId, HitMap, HitRegion, HitRole};
    pub use crate::image::{
        ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
    };
    pub use crate::image_scene::{ImageScene, ImageSceneDelta};
    pub use crate::input::{
        TextInput, TextInputEnterBehavior, TextInputKeyHandler, TextInputKeyOutcome,
        TextInputProjection,
    };
    pub use crate::layout::{
        Breakpoint, Breakpoints, Constraint, Direction, DockAreas, DockLayout, Layout, Responsive,
        Split, split,
    };
    pub use crate::list::{List, ListItem, ListKeyHandler, ListKeyOutcome, ListState};
    pub use crate::overlay::{OverlayLayer, OverlayStack};
    pub use crate::palette::{
        CommandPalette, CommandPaletteKeyOutcome, CommandPaletteState, PaletteItem,
    };
    pub use crate::picker::{Dropdown, ListPicker, ListPickerAreas};
    pub use crate::retained_list::{RetainedListLayout, RetainedListLine, RetainedListSignature};
    pub use crate::retained_sectioned_list::{
        RetainedSectionedListLayout, RetainedSectionedListLine,
    };
    pub use crate::style::{Color, Modifier, Style};
    pub use crate::terminal::{DrawStats, Terminal};
    pub use crate::text::{Line, Span, Text};
    pub use crate::text_block::{Alignment, TextBlock, TextWrap};
    pub use crate::text_width::{
        display_width, truncate_to_display_width, wrap_text_with_continuation,
    };
    pub use crate::viewport::{Viewport, ViewportKeyHandler, ViewportKeyOutcome, ViewportState};
    pub use crate::widget::{StatefulWidget, Widget};
}
