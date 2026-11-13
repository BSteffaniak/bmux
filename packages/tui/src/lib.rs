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
pub mod capabilities;
pub mod chrome;
pub mod clock;
pub mod component;
pub mod composition;
pub mod damage;
pub mod embedded_surface;
pub mod event;
pub mod focus;
pub mod frame;
pub mod geometry;
pub mod gesture;
pub mod gradient;
pub mod grid_fragment;
pub mod hit;
pub mod image;
pub mod image_scene;
#[cfg(feature = "terminal-io")]
pub mod inline;
pub mod inline_output;
pub mod input;
pub mod interaction;
pub mod layout;
pub mod line_frame;
#[cfg(target_os = "linux")]
pub mod linux_input;
pub mod measured_list;
pub mod paint;
pub mod pointer;
pub mod presentation;
pub mod selection;
pub mod semantic;
pub mod sensitive_input;
pub mod source_map;
pub mod style;
pub mod terminal;
#[cfg(feature = "terminal-io")]
pub mod terminal_io;
pub mod text;
pub mod text_block;
pub mod text_width;

/// Common imports for building BMUX TUI surfaces.
pub mod prelude {
    pub use crate::ansi::{
        AnsiFrameDiffStats, ansi_to_lines, write_ansi_frame, write_ansi_frame_diff,
    };
    pub use crate::buffer::{Buffer, Cell};
    pub use crate::capabilities::{
        TerminalBackground, TerminalCapabilities, TerminalColorDepth, TerminalColorReport,
    };
    pub use crate::chrome::{Border, BorderSet, BorderSide, BorderSideStyles, BorderSides};
    pub use crate::clock::PresentationClock;
    pub use crate::component::{
        ChildLayout, Component, ComponentRevision, Constraints, Element, ElementObject, EventCx,
        IntoElementObject, LayoutCache, LayoutCacheStats, LayoutCx, LayoutEnvironment, LayoutId,
        LayoutMetadata, LayoutNode, LogicalRect, LogicalSize, SendElement, SyncElement,
        combine_child_revisions,
    };
    pub use crate::composition::{
        Align, Clip, Column, Fill, Flex, HitArea, HorizontalAlignment, Keyed, Padding, Row,
        SizeBox, Stack, StyleScope, Surface, TextBlock, TextProjectionRow, TextProjectionWindow,
        TextSourceRows, VerticalAlignment, Visibility,
    };
    #[cfg(feature = "terminal-io")]
    pub use crate::terminal_io::{
        TerminalGuard, event_from_terminal, key_from_terminal, mouse_from_terminal, poll_event,
        read_event, terminal_size,
    };
    pub use crate::damage::{Damage, DamagePolicy};
    pub use crate::event::{
        Event, EventHandler, EventOutcome, FocusEvent, MouseButton, MouseEvent, MouseEventKind,
        MouseModifiers,
    };
    pub use crate::focus::{FocusId, FocusKeyOutcome, FocusTrap};
    pub use crate::frame::{Cursor, Frame};
    pub use crate::geometry::{Insets, Point, Rect, Size};
    pub use crate::gesture::{ClickSlop, GestureRoute, PointerGesture, PointerGestureRecognizer};
    pub use crate::hit::{Hit, HitId, HitMap, HitRegion, HitRole};
    pub use crate::image::{
        ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
    };
    pub use crate::image_scene::{ImageScene, ImageSceneDelta};
    pub use crate::interaction::{InteractionRoute, InteractionRouter};
    pub use crate::layout::{
        Breakpoint, Breakpoints, Constraint, Direction, DockAreas, DockLayout, Layout, Responsive,
        Split, split,
    };
    pub use crate::line_frame::{
        FramedLines, LineFrame, LineFrameGeometry, LineFrameOverflow, LineFrameWidth,
    };
    pub use crate::measured_list::{MeasuredListIndex, MeasuredListItem, VisibleItemRange};
    pub use crate::paint::{LocalRect, PaintCx};
    pub use crate::pointer::{PointerIntent, PointerShape, resolve_pointer_intent};
    pub use crate::selection::{
        SelectionAffinity, SelectionAutoScrollPolicy, SelectionAutoScrollRequest, SelectionCapture,
        SelectionCell, SelectionContentId, SelectionController, SelectionEndpoint,
        SelectionFragment, SelectionFragmentId, SelectionGesturePhase, SelectionOutcome,
        SelectionScene, SelectionSceneError, SelectionScope, SelectionScopeId, SelectionScrollAxis,
        SelectionScrollDirection, SelectionSlice, SelectionSnapshot, SelectionTextRun,
        paint_selection_highlights, painted_text_cells, painted_text_fragments,
        plain_text_fragments, plain_text_fragments_with_tabs,
    };
    pub use crate::source_map::{OffsetSourceRows, SourceMapped};
    pub use crate::style::{Color, Easing, Modifier, Style};
    pub use crate::terminal::{AppliedScroll, DrawStats, ScrollRegionAcceleration, Terminal};
    pub use crate::text::{
        Line, Span, Text, TextWrap, TextWrapGeometry, TrailingAlign, TrailingPlacement, wrap_text,
    };
    pub use crate::text_block::Alignment;
    pub use crate::text_width::{
        display_width, truncate_to_display_width, wrap_text_with_continuation,
        wrap_text_with_continuation_character,
    };
}
