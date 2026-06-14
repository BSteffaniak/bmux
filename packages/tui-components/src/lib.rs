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
pub mod badge;
pub mod breadcrumbs;
pub mod button;
pub mod checkbox;
pub mod common;
pub mod dialog;
pub mod empty_state;
pub mod filtered_list;
pub mod form;
pub mod form_field;
pub mod key_hint_bar;
pub mod labeled_details;
pub mod menu;
pub mod modal_frame;
pub mod pane;
pub mod panel_group;
pub mod picker_frame;
pub mod progress_bar;
pub mod radio_group;
pub mod scroll_area;
pub mod select_dropdown;
pub mod selectable_list;
pub mod sparkline;
pub mod status_bar;
pub mod tab_bar;
pub mod table;
#[cfg(feature = "text-input")]
pub mod text_input;
#[cfg(feature = "text-input")]
pub mod text_input_box;
pub mod text_view;
pub mod tree_view;
