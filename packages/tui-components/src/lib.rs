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
pub mod checkbox;
pub mod common;
pub mod dialog;
pub mod filtered_list;
pub mod form;
pub mod form_field;
pub mod labeled_details;
pub mod menu;
pub mod modal_frame;
pub mod pane;
pub mod panel_group;
pub mod radio_group;
pub mod scroll_area;
pub mod select_dropdown;
pub mod selectable_list;
#[cfg(feature = "text-input")]
pub mod text_input;
#[cfg(feature = "text-input")]
pub mod text_input_box;
