//! Source-independent presentation content and canonical BMUX component composition.
//! Hosts own state, asset authorization, effects, reload IO, and transport sanitization.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

pub mod builder;
pub mod component;
pub mod custom;
pub mod markdown;
pub mod model;
pub mod navigation;
pub mod theme;
pub mod viewer;

pub use builder::{DeckBuilder, SlideBuilder};
pub use component::{HiddenContent, ImageComponent, SlideComponent, SpeakerNotesComponent};
pub use custom::{CustomComponentFactory, CustomRegistry, OwnedFactory, RichContent};
pub use markdown::{MarkdownError, parse_markdown};
pub use model::{
    Block, BlockId, BlockKind, Column, Deck, DeckId, Metadata, Slide, SlideId, SourceLocation,
    SpeakerNotes, TextFormat,
};
pub use navigation::{NavigationAction, NavigationOutcome, PresentationState, reduce};
pub use theme::PresentationTheme;
pub use viewer::{ViewerComponent, ViewerState};
