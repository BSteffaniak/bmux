#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Reusable BMUX TUI components.
//!
//! This crate intentionally layers higher-level controls on top of raw
//! [`bmux_tui`] primitives instead of replacing them. Component state is kept
//! separate from component policy so applications can opt into behavior one
//! feature at a time.
//!
//! Every public component is opt-in. Composed component features enable their
//! implementation prerequisites, and the additive `all` feature exists for
//! galleries, documentation, and comprehensive validation.

#[cfg(feature = "action-row")]
pub mod action_row;
#[cfg(feature = "badge")]
pub mod badge;
#[cfg(feature = "bar-chart")]
pub mod bar_chart;
#[cfg(feature = "breadcrumbs")]
pub mod breadcrumbs;
#[cfg(feature = "button")]
pub mod button;
#[cfg(feature = "canvas")]
pub mod canvas;
#[cfg(feature = "chart")]
pub mod chart;
#[cfg(feature = "checkbox")]
pub mod checkbox;
pub mod common;
#[cfg(feature = "compact")]
pub mod compact;
#[cfg(feature = "dialog")]
pub mod dialog;
#[cfg(feature = "diff-viewer")]
pub mod diff_viewer;
#[cfg(feature = "empty-state")]
pub mod empty_state;
#[cfg(feature = "form")]
pub mod form;
#[cfg(feature = "form-field")]
pub mod form_field;
pub mod hit_test;
#[cfg(feature = "key-hint-bar")]
pub mod key_hint_bar;
#[cfg(feature = "labeled-details")]
pub mod labeled_details;
#[cfg(feature = "menu")]
pub mod menu;
#[cfg(feature = "modal-frame")]
pub mod modal_frame;
#[cfg(feature = "pane")]
pub mod pane;
#[cfg(feature = "panel-group")]
pub mod panel_group;
#[cfg(feature = "picker-frame")]
pub mod picker_frame;
#[cfg(feature = "progress-bar")]
pub mod progress_bar;
#[cfg(feature = "radio-group")]
pub mod radio_group;
#[cfg(feature = "scroll-view")]
pub mod scroll_view;
#[cfg(feature = "scrollbar")]
pub mod scrollbar;
#[cfg(any(feature = "selectable-list", feature = "table", feature = "text-view"))]
pub mod scrollbar_layout;
#[cfg(feature = "select-dropdown")]
pub mod select_dropdown;
#[cfg(feature = "selectable-list")]
pub mod selectable_list;
pub mod selection;
#[cfg(feature = "source-viewer")]
pub mod source_viewer;
#[cfg(feature = "sparkline")]
pub mod sparkline;
#[cfg(feature = "status-bar")]
pub mod status_bar;
#[cfg(feature = "stepper")]
pub mod stepper;
#[cfg(feature = "tab-bar")]
pub mod tab_bar;
#[cfg(feature = "table")]
pub mod table;
#[cfg(feature = "terminal-viewer")]
pub mod terminal_viewer;
#[cfg(feature = "text-input")]
pub mod text_input;
#[cfg(feature = "text-input-box")]
pub mod text_input_box;
#[cfg(feature = "text-view")]
pub mod text_view;
pub mod theme;
#[cfg(feature = "toast-stack")]
pub mod toast_stack;
#[cfg(feature = "tree-view")]
pub mod tree_view;
#[cfg(feature = "virtual-list")]
pub mod virtual_list;
