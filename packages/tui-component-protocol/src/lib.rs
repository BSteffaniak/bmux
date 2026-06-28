//! Serializable declarative TUI component protocol for BMUX surfaces.
//!
//! This crate defines protocol data only. Runtime crates render these models
//! with [`bmux_tui`] and [`bmux_tui_components`], while plugins and hosts can
//! exchange component trees without sharing concrete widget implementations.
//! Serialization support is opt-in through feature-gated modules:
//!
//! * `serde` derives serde traits on protocol models.
//! * `serde-json` enables JSON helper functions.
//! * `bmux-codec` enables BMUX codec helper functions.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub mod event;
pub mod ids;
pub mod model;
#[cfg(any(feature = "serde-json", feature = "bmux-codec"))]
pub mod serialization;
pub mod state;
pub mod value;

pub use event::{ComponentEvent, ComponentEventKind};
pub use ids::{ActionId, ComponentId};
pub use model::{
    ButtonRole, CheckboxOption, ComponentKind, ComponentNode, ComponentTree, InputKind, OptionItem,
    PanelChrome, StackDirection, StatusLevel, TextAlign,
};
pub use state::{ComponentRuntimeState, FocusState};
pub use value::ComponentValue;
