#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Reusable BMUX TUI components.
//!
//! This crate intentionally layers higher-level controls on top of raw
//! [`bmux_tui`] primitives instead of replacing them. Component state is kept
//! separate from component policy so applications can opt into behavior one
//! feature at a time.

pub mod action_row;
pub mod button;
pub mod common;
pub mod labeled_details;
pub mod modal_frame;
pub mod pane;
#[cfg(feature = "text-input")]
pub mod text_input;
